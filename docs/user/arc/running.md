---
description: The commands that run a task: start, cancel, reset-budget and reverify.
---

# arc: running a task

The commands that run, end and re-check a task.

## `radian arc start <TASK>`

`start` runs one attempt of one task and waits until it ends.
It is the owner's way to run one task: `radian arc run` runs every task the auto levels allow.
A task already in `review.verifying` (one a moved branch or a changed digest sent back) resumes verification instead: `start` runs the gate, prints its line, claims no new attempt, and exits `3` if verification cannot run.
When a scheduling loop is already running that task's gate, `start` waits for it and reuses its verdicts.

```text
radian arc start <TASK>
```

| Argument | Value | What it does |
|---|---|---|
| `<TASK>` | `ID` | The task id. Required. |

Before it reconciles, claims or verifies anything, `start` checks the worker line (see the [`worker.exec_args` and `worker.command` rows](config.md#configtoml)).
Without `[worker] command`, it parses the line the worker would get: the wrapper's fixed `radian acp` arguments plus `[worker] exec_args`.
A line `radian acp` would reject, or one that repeats an argument the wrapper sets, refuses the command with exit `3` and the parser's own message, names `worker.exec_args`, claims nothing, and changes no file.
With `[worker] command` set, arc checks instead that the program resolves (see [Another agent as the worker](worker.md#another-agent-as-the-worker)); a program that does not resolve refuses the command the same way, naming `worker.command` and the program. A flow's `acp` or `command` program is checked the same way and named by its own key.

Before it claims anything it reconciles every task in `doing` (see [Reconcile](gate.md#reconcile)) and moves a `todo.retry_wait` task whose `retry_at` has passed back to `todo.ready`.

`start` also claims the task you name when it is `todo.retry_wait`, even while its `retry_at` is still in the future: your `start` is a run-now, and the backoff exists for the scheduling loop. The claim is the same transaction as a `todo.ready` one, with the same concurrency check and the same printed line, and it clears `retry_at`.

A claim is refused with exit `3` and changes nothing when:

- the task is not `todo.ready` or `todo.retry_wait`; a `todo.proposed` task names its phase and waits for `radian arc confirm`, and `todo.paused_budget` and every `failed.*` phase are refused;
- its plan is not `confirmed`; a `cancelled` plan is refused, naming its status;
- an `after` task is not `done`;
- its flow already has [`flows.<flow>.concurrency`](config.md#configtoml) tasks in `doing`;
- [`limits.concurrency`](config.md#configtoml) tasks are already in `doing`.

The refusal names the full limit and its count: `flows.<name>.concurrency` or `limits.concurrency`. When both are full, it names the flow's limit.

The claim is one transaction: the task's `attempt` goes up by one, `attempt_id` becomes `<task>-<attempt>`, a random `token` is written, and the task moves to `doing.starting`.
arc then runs the hidden `radian arc work` wrapper as a child, with the brief on its stdin; the wrapper moves the task to `doing.running`.
The worker itself — the session it runs in, the brief it receives, the steps before an attempt, the limits the wrapper enforces, and the checks after it exits — is in [the worker](worker.md).
When [`worker.command`](config.md#configtoml) is set, another agent runs in its place: see [Another agent as the worker](worker.md#another-agent-as-the-worker). A flow can also start an ACP agent with [`flows.<name>.acp`](config.md#configtoml); see [An ACP agent as the worker](worker.md#an-acp-agent-as-the-worker).

When the attempt ends, arc applies the result in one transaction (the [result table](gate-failures.md#failures-retries-and-the-budget)) and prints one line:

```text
<attempt id> <outcome> -> <state>.<phase>
<attempt id> <outcome> -> <state>.<phase> (retry at <UTC time>)
<attempt id> <outcome> -> <state>.<phase> (<reason>)
```

For example `TK0001-work-1 completed -> review.verifying`.
A result whose token is not the task's current attempt prints `<attempt id> ignored: <why>` and changes nothing.
Before its own line, `start` prints `reconciled: <line>` for each task the reconcile step changed.

A pushed `completed` attempt continues into the gate in the same process, so one `start` takes a task from `todo.ready` (or `todo.retry_wait`) to `review.awaiting_human` or `review.rework_pending`.
The verification prints its own line:

```text
<attempt id> verify -> <state>.<phase>
<attempt id> verify -> <state>.<phase> (<stage>: <reason> (verdict <path>))
```

For example `TK0001-work-1 verify -> review.awaiting_human`, or `TK0001-work-1 verify -> review.rework_pending (commands: command_failed (verdict /…/verdicts/commands.json))`. A review whose model is unavailable prints `<task> review model unavailable -> retry at <UTC time>` instead (see [A review wait on an unavailable model](gate-review.md#a-review-wait-on-an-unavailable-model)).
When verification cannot run (a moved branch, a stage this build does not have), the task stays in `review.verifying`, `start` prints the attempt's own line and then the reason on stderr, and exits `3`.

`start` exits `0` when the task left `doing`, `1` when it is still in `doing` after the attempt (or on an internal error), `3` when the claim is refused or verification refuses, and `4` when another process holds `state.lock`.
`start` waits for the wrapper, so it exits only after the attempt ends; `radian arc run` does not wait.

## `radian arc cancel <ID>`

`cancel` ends one task, or ends every task it accepts in a plan and the plan itself.
It is your action (there is no MCP tool for it), and it reconciles `doing` tasks first, as `start` does.

| Argument | Value | What it does |
|---|---|---|
| `<ID>` | a plan or task id | Required. An id that names both a plan and a task is refused (`3`), naming both files; an id that names neither exits `1`. |

### Task form

| The task is | What `cancel` does |
|---|---|
| `todo` (any phase) | moves it to `failed.cancelled` |
| `doing`, starting or running | SIGTERM to the wrapper, then SIGKILL after `[limits] cancel_grace_secs` plus a one-second window for the wrapper to reap what it stopped and write its record; when the wrapper's `AttemptFinished` arrives the task goes to `failed.cancelled`; when it never arrives the task goes to `doing.reconciling` |
| `doing.reconciling` | checks the wrapper's pid and start time; signals it if it is still that process; moves the task to `failed.cancelled` |
| `review` (any phase) | stops a gate that is running for the task, then moves the task to `failed.cancelled` |
| `failed.exhausted`, `failed.irrecoverable` | moves the task to `failed.cancelled`: you are saying "seen, given up". A stale gate record or lock, if one exists, is checked and stopped the way a `review` task's gate is; no other process runs for the task. |
| `done` (any phase), `failed.cancelled` | refused with exit `3`, with a message that lists the phases `cancel` accepts |

A task that ran out of attempts (`failed.exhausted`) or cannot continue (`failed.irrecoverable`) has no worker, so `cancel` only moves it to `failed.cancelled`.
The task's attempt history, its attempt id, its verdicts and its `total_cost_usd` stay as they are; only the phase changes.
A `done.merged` task and a task already at `failed.cancelled` are refused.

When a gate is running for a `review` task (its `runs/<task>/gate.json` record is `started` or `running`), `cancel` signals the gate process the same way a scheduling-loop stop does: SIGTERM, `[limits] cancel_grace_secs`, then SIGKILL, and the gate's record becomes `interrupted`.
A `failed.exhausted` or `failed.irrecoverable` task has no gate of its own, but a stale record or lock from an earlier attempt is handled by the same process check: `cancel` signals only the process the record names, and only while it is still that process.
The task moves only after the gate is stopped.

The wrapper's pid and its start time both come from the attempt record, so a pid the operating system reused is never signalled.
`cancel` prints `<task> <state>.<phase>`, for example `TK0001-work failed.cancelled`.

### Plan form

A plan id runs these steps in this order:

1. the plan's `auto` becomes `off`;
2. every task of the plan in `todo`, `doing` or `review`, and every task at `failed.exhausted` or `failed.irrecoverable`, is cancelled by the task form's rules;
3. a task in `done` or `failed.cancelled` is left as it is;
4. the plan's `status` becomes `cancelled`.

A plan cancelled this way has every task it could act on at `done.merged` or `failed.cancelled` — a task file that changed outside arc is left as it is — so a plan whose only non-merged task was exhausted reads finished (`✓`) after the cancel, when at least one task merged.

A task a gate is running for is stopped as the task form stops it.
If a task's cancel leaves it `doing.reconciling`, the plan still becomes `cancelled`; the next `radian arc run` pass ends the task at `failed.cancelled`, after the same process check the task form makes (see [Reconcile](gate.md#reconcile)).
That pass waits no second grace: the plan form already sent the wrapper its SIGTERM, so the pass checks the wrapper's pid and start time and kills it at once when it is still there.
A task the lead adds to the plan while these steps run is cancelled too: the plan's tasks are read once more after the status becomes `cancelled`, and a task added after the first read is ended by the task form's rules.
A plan that is already `cancelled` — by its `status`, or by all its tasks being `failed.cancelled` — is refused with exit `3`.

`cancel` prints one `<task> <state>.<phase>` line for each task it changed, in the plan's task order, then `<plan> cancelled`:

```text
TK0002-b failed.cancelled
TK0003-c failed.cancelled
PL0001-plan cancelled
```

A cancelled plan is final.
`cancel`, `confirm`, `auto`, `priority`, `start`, `reverify`, `reset-budget`, `add task`, and the MCP `add_task`, `update_task`, `update_plan` and `add_rework` tools refuse it with exit `3` (the MCP tools: an error naming the status).
`radian arc run` never claims, gates or merges its tasks, and the summary does not list it as waiting.
`list` hides it unless `--all`, and `show <plan>` and `show <task>` still read it and its tasks.

`cancel` exits `0` when it ended the task or plan or left a task `doing.reconciling`, `1` when the id names nothing, `3` for a refused state or an id that names both a plan and a task, and `4` when another process holds `state.lock`.

## `radian arc reset-budget <ID>`

`reset-budget` returns budget-paused tasks to `todo.ready` and resets the counted cost, `cost_usd`, to zero from now on. The task's `total_cost_usd` does not change: it is what the task has spent in total, not the budget counter.
The id names one plan or task; an id that names both is refused with exit `3` (see [Plan and task files](files.md#plan-and-task-files)).

| Argument | Value | What it does |
|---|---|---|
| `<ID>` | a plan or task id | Required. A plan id resets every paused task of that plan. |

- Task form: the task must be `todo.paused_budget`; it moves to `todo.ready`, its `cost_usd` becomes `0`, and its `total_cost_usd` stays. Any other state is refused (`3`).
- Plan form: every task of the plan that is `todo.paused_budget` moves to `todo.ready`; every task of the plan gets `cost_usd` `0`, because the plan's budget counts all of them. `total_cost_usd` stays on every task. The plan's `auto` level is not changed.

A `cancelled` plan is refused with exit `3`, in the plan form and in the task form.

It prints `<id>: <n> task(s) ready` and exits `0`.
An id that names no plan or task exits `1`.

## `radian arc reverify <TASK>`

`reverify` re-runs the gate for a task's already pushed commit without running a worker.
It is your action (there is no MCP tool for it), and it exists for a gate that was wrong: a missing build step, a flaky test, or a gate command fixed after the verdict was written.

```text
radian arc reverify <TASK>
```

| Argument | Value | What it does |
|---|---|---|
| `<TASK>` | `ID` | The task id. Required. It must be `review.rework_pending`, `review.awaiting_human`, or `review.verifying` with a stuck gate. |

It applies only when the task's `rework_reason`, if there is one, is one a re-run can fix:

- a failed verification stage: `commands`, `mutation`, `review`, or `ci`, with any reason;
- the integration stage failed with reason `command_failed` or `timeout`, the same stage and reason pair the automatic reverify of a moved target branch accepts (see [A command failure and a moved target branch](gate-failures.md#a-command-failure-and-a-moved-target-branch)); the target branch does not need to have moved.

A reject note, a failed delivery check, an integration failure with any other reason (a `conflict` has its own rework path, in [A conflict with the target branch](gate-failures.md#a-conflict-with-the-target-branch)), and every other state are refused with exit `3`.
A task in `review.verifying` is accepted when its gate is stuck: its `runs/<task>/gate.json` record names the task's current attempt with status `no_record` or `died` (see [the gate run](gate.md#the-gate-run)), and no gate process holds the task's gate lock.
Any other `review.verifying` task, including one whose gate is still running, is refused with exit `3`.
A plan paused by the automatic rework limit does not block `reverify` when the task's own reason is one of the failures above.

A task whose `commands` or integration stage failed with `command_failed` or `timeout` is also re-verified by `radian arc run` once the target branch moved (see [A command failure and a moved target branch](gate-failures.md#a-command-failure-and-a-moved-target-branch)). A task waiting on an unavailable review model is re-verified by `radian arc run` once its `retry_at` passes (see [A review wait on an unavailable model](gate-review.md#a-review-wait-on-an-unavailable-model)).
A task of a `cancelled` plan is refused with exit `3`.
`reverify` ignores the automatic path's two guards: it runs whether or not the `(head_sha, target branch)` pair was re-verified before, and whether or not `limits.max_auto_reverify` is used up; it does not use up the count.

A reverify claims a new attempt without a worker:

- `attempt` goes up by one, and `attempt_id` and `token` are new;
- `head_sha` and `base_sha` stay the previous attempt's, and nothing is pushed;
- the attempt result's class is `reverify`;
- the task moves to `review.verifying` and is verified at once in the same command, as `start` does after a pushed attempt.

The new attempt's verdicts are its own: an earlier attempt's verdict is not reused and no verdict is ever rewritten.
The remote `work/<task>` must still equal `head_sha`; when it moved, `reverify` exits `3` with the moved-branch reason and the task stays in `review.verifying`, as verification does.

A reverify attempt is not a retry: it changes neither `retry_count` nor `max_attempts` and is not an automatic rework (`auto_rework` does not change).
It runs no worker, so no worker cost is added, but the `review` stage it runs counts: its cost is added to the task's `cost_usd` and `total_cost_usd` when the gate's verdicts are applied, and it can pause the task in `todo.paused_budget` (see [`limits.max_cost_usd`](config.md#configtoml)).

It prints the claim's line and then the verification's:

```text
<attempt id> reverify -> review.verifying
<attempt id> verify -> <state>.<phase>
```

For example `TK0001-work-2 reverify -> review.verifying`, then `TK0001-work-2 verify -> review.awaiting_human`.
A gate stage that fails again is a normal result: the task moves to `review.rework_pending`, the verification line carries the stage and its verdict, and `reverify` exits `0`.

| Code | Meaning | When |
|---|---|---|
| `0` | success | The verification ran, whether it passed or a stage failed. |
| `1` | an error | A git or file error. |
| `3` | the state does not allow the operation | The task is in another state, its rework reason is not one of the failures `reverify` accepts, it has no `head_sha`, the branch moved, or a listed stage is not built in this build. |
| `4` | a lock is held by another process | Another process holds `state.lock`. |

