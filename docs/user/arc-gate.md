---
description: The arc gate: verification stages, the review stage, failures, retries, the budget, and reconcile.
---

# arc: the gate

This page covers the gate run and its failures; the commands are in [`arc`](arc.md), and the files arc keeps are in [`arc-files`](arc-files.md).

## The gate run

A task in `review.verifying` is checked against `verified_sha`, which is the pushed `head_sha` of the task's current attempt.
`start` runs the gate in the same process after a pushed attempt, and `radian arc run` starts it as a background process: a pass starts the gate and returns, and a later pass reads its verdicts and moves the task ([`run`](arc.md#radian-arc-run)).
A gate writes its verdict files and one record, `runs/<task>/gate.json`; it never changes the task's state itself.

- arc fetches `[repository] remote` first.
  The remote's `work/<task>` must still be `head_sha`.
  When it differs, arc records the reason in the attempt's `review.json` and the task stays in `review.verifying`; `show` prints the reason.
- The stages are the configured profile's (`gate.profile`, then `gate.profiles.<name>.stages`), in order, plus the task's `extra_stages`.
  `human` is your approval and does not run here.
- Every stage runs against the verification candidate, in its own checkout under `.radian/arc/gates/<stage>/`.
  The candidate is `verified_sha` when the remote main is an ancestor of it; otherwise it is a merge commit of `verified_sha` onto the remote main, built in that checkout and never pushed.
  The merge uses arc's own commit identity, so the same inputs build the same tree.
  `verified_sha` stays the task's commit: the approval, the digest and `merge` bind to it, and the candidate is evidence only.
  A stage's commands run in order and stop at the first failure; each has its own log and the profile's timeout.
- A verification stage's verdict records `sha` (the task's `verified_sha`), `main_sha` (the remote main the candidate was built on) and `candidate_sha` (the commit the stage ran on; absent when the merge conflicted).
- A merge conflict is a failed stage: the verdict's reason is `conflict`, its `conflict_paths` lists the paths, the task goes to `review.rework_pending`, and `show` prints `conflict: <paths>`.
  Nothing is pushed.
- `review` is a read-only radian session, not the worker session.
  It runs after `commands` passes and before `human`; a profile that lists both must list `commands` first.
  For each run it starts one `radian exec` in its own checkout, `.radian/arc/gates/review/`, on the candidate, with the candidate as the workspace and only the `read`, `grep` and `glob` tools mounted: no shell tool, no write tool, and no MCP server.
  Its model route is `[gate.review] exec_args`, and `[gate.review] timeout_secs` limits one session.
  The prompt is built in: the task's brief, the commit list and the diff from the merge base of the remote main and `verified_sha` to `verified_sha` (the task's own change, not main's), and the instruction to check the change against the brief and the repository's rules (`AGENTS.md` and the design records the brief names).
  `[gate.review] prompt_file` is appended to it.
  The session must end with one fenced `json` block, and arc parses the last such block:

  ```json
  {"summary": "what the change does", "blocking": [], "suggestions": [], "verified": []}
  ```

  `summary` is 2 to 6 sentences. `blocking` lists what must change before the change can merge; an empty list passes the stage. `suggestions` and `verified` never fail the stage.
  Any blocking item fails the stage with reason `review_blocking`; the task goes to `review.rework_pending` and `rework_reason` lists the items.
  No parsable block, a missing field, a timeout, or a session error runs the session once more; when the second run is unusable too the stage fails with reason `review_output`, or `review_timeout` when the last run timed out. Both runs' session ids and their summed cost are in the verdict.
  The review verdict is `runs/<task>/attempts/<attempt id>/verdicts/review.json`; it records `review` (the four fields), `review_session_ids` and `review_cost_micros` beside the usual `sha`, `main_sha` and `candidate_sha`. It is bound to `verified_sha`, is part of `verdict_set_digest`, and `reverify` runs it again.
- `review` failure reasons: `review_blocking` (with its items in `rework_reason`), `review_output` (no usable block after two runs), and `review_timeout` (the last run hit `[gate.review] timeout_secs`).
- This build runs `commands` and `review`.
  A profile or a task that lists `mutation` or `ci` is refused with `stage <name> is not built yet`; the task stays in `review.verifying` and `show` prints that reason.
  `init`'s default profile lists only `commands` and `human`.
- A verdict is `runs/<task>/attempts/<attempt id>/verdicts/<stage>.json`; a command's log is `runs/<task>/attempts/<attempt id>/logs/<stage>-<n>.log`.
  A verdict is written once and never changed.
  A verdict that already exists for this attempt and stage is reused instead of rerun, so a crash between stages resumes where it stopped.
- One gate runs per task at a time.
  The gate process holds `gates/<task>.gate.lock` from the moment a pass starts it, so the pass that follows starts no second gate for the same commit.
  A signalled or killed gate releases the lock; the next pass starts it again because it left its `running` record, and the verdicts already written are reused.
  Stages of different tasks still queue on their own locks: one `commands` stage at a time, one checkout per stage, and the review stage has its own.
  The `review` stage is a read-only session, so it does not wait for another task's `commands`.
- The gate's own output is `runs/<task>/gate.log`.
- The gate records its outcome in `runs/<task>/gate.json` with the attempt it ran for: `done`, `refused` (a check before the stages refused: a moved branch, a stage this build does not run, no `head_sha`), or `error`.
  A pass applies a `done` record; reports a `refused` one in the summary and starts no new gate for that attempt until the inputs change (the branch comes back, `config.toml` changes, or `start`/`reverify` claims a new attempt); and reports an `error` one as an error (exit `1` for a one-shot `run`).
  `radian arc show` keeps printing the recorded reason.
- Before a pass starts a gate it writes the inputs the gate will read to that record (`status` `started`); the gate replaces them with `running`, and an integration gate is the same.
  A gate that exits before writing any record leaves `started`: the next pass records `no_record` for that attempt with those inputs, reports it in the summary, starts no gate while they are unchanged, and does not count the pass as changed. A new `head_sha`, a new main or a changed `config.toml` allows one new start.
  A gate that wrote `running` and then died is started again.
- Any stage failing moves the task to `review.rework_pending`.
  The stage, its reason and the verdict path are in the task's `rework_reason` (`show` prints it).
  Main moving after a verification does not send the task back to `review.verifying` by itself; `merge` integrates onto the then-current main and runs the key commands there.
- Every required stage passing moves the task to `review.awaiting_human` and records `verified_sha` and `verdict_set_digest` in the task.
  The digest covers the verification verdicts for `verified_sha`; it leaves out the `human` approval and the `integration` verdict, which are written later, at merge.
  A new verdict for the same commit changes the digest and invalidates an approval.

`list` and `show` do not use the network: they do not check the remote and cannot see a branch that moved after the gate.
`merge` checks the remote before it acts.

### A conflict with main

A verification stage (`commands`, `mutation`, `review`) or the integration stage can fail with reason `conflict`: the task's commit or branch does not merge with main.
With `merge-main` on, the next attempt starts mid-merge, so arc writes its brief and moves the task to `todo.ready`.
The heading is `## Rework <n> (conflict with main)`, with the lead's `add_rework` numbering.
The text names the main commit (short sha) and the stage, lists the conflicted paths, says the next attempt starts mid-merge, and asks the worker to resolve the conflict keeping both sides' intent, commit the merge, rebuild, run the touched tests, and change nothing else.
Every other reason, and a conflict without `merge-main`, keeps `review.rework_pending`; a `reverify` conflict takes the automatic path above.
It never raises `auto_rework` and never pauses the plan, and it counts as an attempt: a task that keeps conflicting reaches `failed.exhausted` when its last counted attempt conflicts.
`auto` decides whether the next attempt starts by itself.

### A command failure and a moved main

A `commands` stage, or an integration key command, can fail because of the tree rather than the task: a flaky test, or a main that is broken itself.
When the remote main is no longer the main the failing verdict recorded, `radian arc run` re-verifies the task by itself, the way `radian arc reverify` does: it claims a new attempt without a worker and starts its gate in the background ([`run`](arc.md#radian-arc-run)).

It applies only when all of these hold:

- the task's plan has `auto` other than `off`;
- the task is `review.rework_pending` because a `commands` or integration stage failed with reason `command_failed` or `timeout`;
- the remote main is not the `main_sha` the failing verdict recorded; an integration verdict records that main as `base_sha`.

A review stage failure, a conflict, a reject note, a failed delivery check, and a task the lead returned to `todo.ready` are never re-verified by themselves.

Two guards stop a loop:

- each `(head_sha, remote main)` pair is re-verified by itself at most once, and the pairs are kept in `runs/<task>/auto_reverify.json`. A main that does not move runs nothing again.
- [`limits.max_auto_reverify`](arc-files.md#configtoml) counts the automatic reverifies of one task. When the count is used up, the task stays in `review.rework_pending`, `radian arc run` prints one line with the reason, and `show` prints the same text as the `rework_reason`. The plan is not paused.

An automatic reverify starts no worker, does not count toward `max_attempts`, and does not count toward `auto_rework`.
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

With `ARC_GATE_HOST` set, `full` ships `HEAD` as a git bundle (`HEAD --not origin/main`; no bundle when `HEAD` is already in `origin/main`). On that host it takes a blocking lock on `~/.arc-gate.lock`, fetches the bundle, checks out exactly that commit detached inside `ARC_GATE_TREE`, and runs that checkout's `scripts/verify.sh`.

| Environment variable | Type | Default | What it does |
|---|---|---|---|
| `ARC_GATE_HOST` | string | unset: `full` runs in this checkout | The ssh host that runs the full gate. |
| `ARC_GATE_TREE` | string | unset | Required when `ARC_GATE_HOST` is set: the gate checkout on that host, as a path relative to the remote home directory. |

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

A retry moves the task to `todo.retry_wait` with `retry_at` = now + a backoff: 10 seconds for the first attempt, doubled for each later attempt, capped at 5 minutes.
When `attempt` has reached the task's `max_attempts` (else `[limits] max_attempts`), the task goes to `failed.exhausted` instead.
`todo.retry_wait` becomes `todo.ready` when `retry_at` has passed; `start` does this before it claims.

Every attempt of a task runs in one worker session, whose id is the task id.
The first attempt that reaches the model creates the session; every later attempt, a retry or a rework, resumes it.
An attempt that failed before the model started (a missing credential, a worktree that could not be prepared) leaves no session, so the next attempt creates it.
A bad `worker.exec_args` entry no longer gets that far: `start`, `run` and `confirm` refuse it before the claim.

Cost is counted per attempt.
`Result.cost` is the session total, so the attempt's cost is this attempt's session total minus the previous attempt's.
The delta is added to the task's `cost_usd`; the attempt result records `cost_usd` and `session_cost_usd`.
A `reverify` attempt runs no worker and costs nothing: it adds nothing to `cost_usd`.

When a retry is due and the budget is used up, the task goes to `todo.paused_budget` instead of `todo.retry_wait`:

- the task's `max_cost_usd`, else `[limits] max_cost_usd`;
- the plan's `max_cost_usd`, counted over every task of the plan. A plan budget also sets the plan's `auto` to `off`.

`reset-budget` is the only way back.

## Reconcile

arc keeps no attempt state in memory.
Before `start` or `cancel` acts, and after a restart, it reconciles every task in `doing`:

- a wrapper that failed after its attempt directory existed wrote its own end record before it exited, whatever the failure: a refused argument, a worktree lock another attempt holds, a fetch that failed, a worktree that could not be prepared, or a worker that could not start. The record is applied at once: the attempt's concurrency slot is free in the next pass, the class is an infrastructure failure, and the retry rules apply. A retry is not counted as a rework;
- only a failure before those directories exist (a task id that names no path, a directory that cannot be created) leaves no record. That attempt is applied as an infrastructure failure when its heartbeat is older than the stall window, as below;
- the wrapper's `attempt_finished.json` is present and its token is current: the result is applied;
- the wrapper's last heartbeat (`attempts/<task>/<attempt>/heartbeat.json`) is younger than the attempt's stall window (its `stall_timeout_secs`, else `[limits] stall_timeout_secs`): the attempt is alive and left alone.
  The wrapper beats at most every 15 seconds and at least three times inside the attempt's stall window, from the moment the attempt directory exists until both end records are written; it keeps beating while the wrapper sets up the worktree, waits for the model, or checks and pushes the result.
  When both stall values are `0` the window is three beats, 45 seconds.
  A claim whose wrapper has not beaten yet is judged by its `start.json`;
- the heartbeat is older than that window and there is no result: the attempt is an `infrastructure` (terminated) failure and the retry rules above apply.

Reconcile never starts a task again.
It reads no process list; `ps` is refused inside the `workspace` sandbox and is never needed here.
`doing.reconciling` is left for you to resolve; `radian arc cancel` moves it to `failed.cancelled` after the same process check.
