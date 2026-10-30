---
description: The gate's failure classes, the retry rules, the budget, and the worker, conflict and moved-branch waits.
---

# arc: failures, retries and the budget

The gate's failure classes and the budget.

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

A worker attempt that ends on an unavailable model waits the same way the review stage does; see [A worker wait on an unavailable model](#a-worker-wait-on-an-unavailable-model).

### A worker wait on an unavailable model

A command worker whose failed attempt's last `Result` carries `model_error.kind` `unavailable` or `usage_limit` did not fail on the request: the task waits on the model's window and uses no retry.
arc records that `model_error` object on the attempt (`runs/<task>/attempts/<attempt id>/attempt_result.json`, with `model_wait` `true`), moves the task to `todo.retry_wait`, and leaves `retry_count` alone. The task therefore never reaches `failed.exhausted` from these waits, whatever `max_attempts` says.

The wait is the task's `retry_at`: 5 minutes for the first model wait of the task, doubled for each further one in a row, capped at 60 minutes; when the failing session's `model_error.retry_after_secs` is longer than that step, it is used instead, still capped at 60 minutes.
The next attempt that does not end in a model wait ends the run of waits.
Every other failure keeps the class, backoff and count in the table above, and so does a model error of another kind (`protocol`, `context_overflow`, …): that one is the request's fault.
A command worker (see [Another agent as the worker](worker.md#another-agent-as-the-worker)) is unchanged, because its failures keep the ordinary retry rules; the radian worker's ACP response carries its model error as `_meta.radian.error` text with no `kind`, so it takes those rules too.

The ordinary pass that moves a `todo.retry_wait` task whose `retry_at` has passed back to `todo.ready` ends the wait, and the next pass claims the task like any retry.
`radian arc run` prints `<task> worker model unavailable -> retry at <UTC time>` when it applies the wait (see [the loop's lines](loop.md#radian-arc-run)); `radian arc show` prints the attempt's `attempt_model_error` and the task's `retry_at`.

A retry moves the task to `todo.retry_wait` with `retry_at` = now + a backoff: 10 seconds for the first retry, doubled for each later one, capped at 5 minutes.
`max_attempts` (the task's, else [`limits.max_attempts`](config.md#configtoml)) counts retries, not attempts: every retry raises the task's `retry_count`, and when the count has reached the limit the task goes to `failed.exhausted` instead.
Every rework sets `retry_count` back to `0` (the lead's brief and arc's review rework), so the new instructions get their own retries.
A conflict rework keeps the count: it continues the same line of work, and the failures between conflicts then use up `max_attempts` even while a conflict keeps returning the task to `todo.ready`.
The attempt number is never compared with the limit; `show` prints `retry_count`.
A `reverify` attempt is not a retry: it changes neither `retry_count` nor the limit.
`todo.retry_wait` becomes `todo.ready` when `retry_at` has passed; the scheduling loop and `start` do this before they claim.
`start` also claims a `todo.retry_wait` task whose time has not come, without waiting for it (see [`start`](running.md#radian-arc-start-task)).

Every attempt of a task runs in a worker session; `fresh` and `resume`, the hand-off section and the records that name the session are in [the session and the brief](worker.md#the-session-and-the-brief).
An attempt that failed before the model started (a missing credential, a worktree that could not be prepared) leaves no session, so the next attempt creates it.
A bad `worker.exec_args` entry no longer gets that far: `start`, `run` and `confirm` refuse it before the claim.

Cost is counted per attempt.
A command worker's `Result.cost` is the session total, so its attempt's cost is the difference from the previous attempt's session total when both ran in the same session (`resume`), and the whole session total when the attempt has a session of its own (`fresh`).
The `radian` worker's `Result.cost` is its prompt turn's fee, so its attempt's cost is that value as it stands, on a resumed session too.
The attempt's cost is added to the task's `cost_usd` and its `total_cost_usd`; the attempt result records `cost_usd` and `session_cost_usd` (the `cost` the worker's `Result` carried).
A `reverify` attempt runs no worker and adds no worker cost, but a `review` stage it runs costs the same as any review.

A `review` stage's cost counts too: when arc applies a gate's verdicts, the sum of the attempt's `review_cost_micros` (every session the stage ran; a run that reported none adds `0`) is added to the task's `cost_usd` and `total_cost_usd`, and both budgets are checked then.
Both counters include every worker attempt and every review; `reset-budget` sets `cost_usd` back to `0` and leaves `total_cost_usd` alone.

When a retry is due and the budget is used up, the task goes to `todo.paused_budget` instead of `todo.retry_wait`:

- the task's `max_cost_usd`, else `[limits] max_cost_usd`;
- the plan's `max_cost_usd`, counted over every task of the plan. A plan budget also sets the plan's `auto` to `off`.

The same check runs when a review verdict is applied: a review cost that reaches the budget stops the task in `todo.paused_budget` after that gate, arc writes no automatic rework for the round, and the task waits for `reset-budget`.

`reset-budget` is the only way back.

### A conflict with the target branch

A verification stage (`commands`, `mutation`, `review`) or the integration stage can fail with reason `conflict`: the task's commit or branch does not merge with the target branch (`[integration] target_branch`, else `[repository] main_branch`).
With `merge-main` on, the next attempt starts mid-merge, so arc writes its brief and moves the task to `todo.ready`.
The heading is `## Rework <n> (conflict with main)`, with the lead's `add_rework` numbering.
The text names the main commit (short sha) and the stage, lists the conflicted paths, says the next attempt starts mid-merge, and asks the worker to resolve the conflict keeping both sides' intent, commit the merge, rebuild, run the touched tests, and change nothing else.
Every other reason, and a conflict without `merge-main`, keeps `review.rework_pending`; a `reverify` conflict takes the automatic path above.
It never raises `auto_rework`, never pauses the plan, and never sends the task to `failed.exhausted` by itself: a conflict rework is not a retry, but it keeps the task's `retry_count` instead of resetting it, so a worker that keeps failing behind repeated conflict reworks still uses up `max_attempts`.

A run of conflicts is bounded by [`limits.max_conflict_rework`](config.md#configtoml) (default `3`).
It is how many conflict reworks arc writes in a row: while it has written fewer than the limit, the conflict gets the brief above; once it has written the limit, arc writes no brief and the task stays in `review.rework_pending`.
`radian arc run` then prints `<task> conflict with main -> review.rework_pending: <reason>`, and `show` prints the same text as part of the `rework_reason`.
An attempt that does not end in a conflict sets the run back to `0`, and so does your own rework (`add_rework`), which is you stepping in; arc's review and conflict reworks do not.
`0` writes none, so the first conflict waits for you; `1` writes exactly one.
`show` prints the task's `retry_count`, not this run.
`auto` decides whether the next attempt starts by itself.

### A command failure and a moved target branch

A `commands` stage, or an integration key command, can fail because of the tree rather than the task: a flaky test, or a target branch that is broken itself.
When the remote target branch is no longer the one the failing verdict recorded, `radian arc run` re-verifies the task by itself, the way `radian arc reverify` does: it claims a new attempt without a worker and starts its gate in the background ([`run`](loop.md#radian-arc-run)).

It applies only when all of these hold:

- the task's plan has `auto` other than `off`;
- the task is `review.rework_pending` because a `commands` or integration stage failed with reason `command_failed` or `timeout`;
- the remote target branch is not the `main_sha` the failing verdict recorded; an integration verdict records it as `base_sha`.

A review stage failure, a conflict, a reject note, a failed delivery check, and a task the lead returned to `todo.ready` are never re-verified by themselves.

Two guards stop a loop:

- each `(head_sha, remote target branch)` pair is re-verified by itself at most once, and the pairs are kept in `runs/<task>/auto_reverify.json`. A target branch that does not move runs nothing again.
- [`limits.max_auto_reverify`](config.md#configtoml) counts the automatic reverifies of one task. When the count is used up, the task stays in `review.rework_pending`, `radian arc run` prints one line with the reason, and `show` prints the same text as the `rework_reason`. The plan is not paused.

An automatic reverify starts no worker, is not a retry (it changes neither `retry_count` nor `max_attempts`), and does not count toward `auto_rework`.
A reverify that fails its gate does not count either, whether `radian arc run` or `radian arc reverify` started it: the count stays as it was and the plan does not pause.
A hand `radian arc reverify` ignores both guards and does not use up the count.

