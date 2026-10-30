---
description: The owner's commands: confirm, auto, priority, pause, merge and reject.
---

# arc: the owner's commands

Your decisions about a plan and a task.

## `radian arc confirm <ID>`

`confirm` is your action: it accepts a draft plan, or a task arc or the lead proposed.

A plan moves from `draft` to `confirmed`.
A confirmed plan gets no new `todo.ready` tasks, and its request text can only gain change records (see [Plan and task files](files.md#plan-and-task-files)).
A plan's `auto` level may only be other than `off` once it is confirmed.

A task moves from `todo.proposed` to `todo.ready`.
The plan's `status` and `auto` do not change, and the plan does not return to `draft`.
A task waits in `todo.proposed` until you confirm it when arc proposed it as the plan's review follow-up (see [The review's follow-up](gate-review.md#the-reviews-follow-up)) or when the lead added it to a confirmed plan with the MCP `add_task` tool; the scheduling loop passes over it, and `start` refuses it.
A proposed task you do not want is ended with `radian arc cancel` (`failed.cancelled`).

| Argument | Value | What it does |
|---|---|---|
| `<ID>` | a plan id, or a task id | Required. An id that names both a plan and a task is refused (`3`), naming both files. |

A plan confirm refuses with exit `3` when the plan has no tasks, when it is already confirmed, when it is cancelled, when its file changed outside arc, and when the worker's `radian acp` line is rejected (see `start` and [`worker.exec_args`](config.md#configtoml)).
A task confirm refuses with exit `3` for a task in any state other than `todo.proposed`, for a task whose plan is `cancelled`, for a file that changed outside arc, and for a rejected worker line.
A refusal writes nothing.

It prints `<plan> confirmed` or `<task> todo.ready`.

### Trying it first: `--dry-run`

| Flag | Value | What it does |
|---|---|---|
| `--dry-run` | — | Run every check the confirm runs, print what it would do, and write nothing. |

`radian arc confirm <ID> --dry-run` runs every check the real `confirm` runs, in the same order, and refuses exactly as `confirm` would: the same message and the same exit code.
It writes nothing: no plan or task file and no ledger entry.
It checks `state.lock` only to find out whether another process holds it, and releases it; it does not create the lock file when it is missing, so no lock is left behind.

On success it prints these lines and exits `0`:

- first, the line the real confirm would print, prefixed `would: `: `would: TK0002-two todo.ready` or `would: PL0001-first confirmed`;
- one `<id> waits on: <chain>` line per task the confirm would make `todo.ready` — the task itself, or a plan's `todo.ready` tasks in the plan's order — with the same entries as `show`'s [`waits_on`](planning.md#the-after-chain), or `<id> waits on: nothing`;
- a `never runs: <id> <state>.<phase>` line for each such task whose chain cannot finish (the same test as `show`'s `never_runs`), and a `cycle: <id> -> … -> <id>` line for a cycle;
- one `<id> queue: <n> of <m> todo.ready` line per such task: its place among every `todo.ready` task plus itself, in claim order (the plan's `priority`, the plan id, the task's order in the plan). A `todo.ready` task no plan lists, possible only through an adopted plan edit, sorts last.

When the next pass would not claim the task, the queue line ends with the reason in parentheses, in the words `radian arc run`'s summary uses: `(waiting for <id> to merge)`, `(waiting for a free <flow> slot)`, `(waiting for a free concurrency slot)`, or ``(waiting for you to start it (`auto` is `off`))``.
The flow reason appears when the task's flow has its [`concurrency`](config.md#configtoml) tasks in `doing` (for example `(waiting for a free image slot)`); the shared reason appears when [`limits.concurrency`](config.md#configtoml) is full, and the flow's reason wins when both are full.
The place is where the task would stand at that moment, not a promise: another task may be claimed or merged before it.

```text
would: TK0002-two todo.ready
TK0002-two waits on: TK0001-one todo.ready
TK0002-two queue: 2 of 2 todo.ready (waiting for TK0001-one to merge)
```

`--dry-run` exits `1` for an id that names nothing, `2` for a malformed id, `3` for a state that does not allow the confirm (an empty plan, an already confirmed plan, a task that is not `todo.proposed`, a file changed outside arc, a cancelled plan, a rejected worker line), and `4` when another process holds `state.lock`.

A real `confirm` prints the same `never runs:` and `cycle:` lines on stderr after its line when they apply.
They are warnings: the confirm still happens and still exits `0`.

## `radian arc auto <PLAN> <LEVEL>`

`auto` sets a plan's `auto` level.

```text
radian arc auto <PLAN> <LEVEL>
```

| Argument | Value | What it does |
|---|---|---|
| `<PLAN>` | `ID` | The plan id. Required. |
| `<LEVEL>` | `off`, `run`, `merge` | The level. Required. |

### The levels

| Level | What arc does | Where it stops |
|---|---|---|
| `off` | Nothing by itself. You start and merge each task. | Each step |
| `run` | Starts each task when its `after` tasks are merged, and runs the gate. | `review.awaiting_human` |
| `merge` | `run`, and merges when every required stage and the integration passed. | Failure, budget, or conflict |

`radian arc run` makes the transitions of these levels; it does not run in the background.
`off` is accepted on any plan; `run` and `merge` need a `confirmed` plan and otherwise exit `3`.
A `cancelled` plan is final: any level, `off` included, is refused with exit `3`.
A value outside the set is a usage error (`2`).

### Automatic reworks

A failed stage or a failed integration sends a task to `review.rework_pending`.
While the task's plan has an `auto` level other than `off`, arc counts that rework in the task's `auto_rework` field.
A failed reverify does not count, whether the loop or you started it.
When the count would go past `[limits] max_auto_rework`, arc pauses the whole plan: its `auto` becomes `off`, the task stays in `review.rework_pending`, and both `radian arc run` and `show` (as part of `rework_reason`) say why.
arc never resets a task's count.
A plan budget pausing the plan also sets `auto` to `off`; `run` starts and merges nothing for a plan at `off`.

## `radian arc priority <PLAN> <PRIORITY>`

`priority` sets a plan's priority: the plan the scheduling loop serves first.

```text
radian arc priority <PLAN> <PRIORITY>
```

| Argument | Value | What it does |
|---|---|---|
| `<PLAN>` | `ID` | The plan id. Required. Any plan, `draft` or `confirmed`. |
| `<PRIORITY>` | `P0`, `P1`, `P2`, `P3`, `P4`, `P5` | The priority. Required. `P0` is the highest and `P3` the default. |

| Priority | Meaning |
|---|---|
| `P0` | The highest: its ready tasks get the next free slot before any lower priority. |
| `P1` | Above the default. |
| `P2` | Above the default. |
| `P3` | The default; a plan whose file has no `priority` field reads as `P3`. |
| `P4` | Below the default. |
| `P5` | The lowest. |

A task has no priority of its own: it follows its plan, in its plan's task order and `after` list.
`priority` only decides which plan gets the next free concurrency slot, the task's flow's or the repository's; it never preempts an attempt that is already running, so a change takes effect at the next pass that has a free slot (see [`run`](loop.md#radian-arc-run)).
The plan's state does not matter: a `draft` plan can be set too.
`list`, `show`, and the MCP `list` and `show` tools print the plan's priority.

A value outside the set (`P6`, `p1`, `high`) is refused with exit `3`, and the message lists every value.
A `cancelled` plan is refused with exit `3`; a refusal writes nothing.

The command prints `<plan> priority <PRIORITY>`, for example `PL0001-first priority P1`.
It exits `0` on success, `3` for a value outside the set or a plan file that changed outside arc, and `4` when another process holds `state.lock`.

## `radian arc pause <PLAN>`

`pause` sets the plan's `auto` level to `off`.

| Argument | Value | What it does |
|---|---|---|
| `<PLAN>` | `ID` | The plan id. Required. |

A task that is already started keeps its state; the next `radian arc run` reads the level before it starts or merges anything.
It exits `0` on a plan that is already `off`.

## `radian arc merge <TASK>`

`merge` takes your approval, builds the integration candidate, lands it on the remote target branch as [`integration.method`](config.md#configtoml) says, and finishes the task.
It is the owner's action (there is no MCP tool for it).
At auto level `merge`, `radian arc run` takes the same approval and starts the integration in the background; a later pass reads its verdict and finishes it.

```text
radian arc merge <TASK>
```

| Argument | Value | What it does |
|---|---|---|
| `<TASK>` | `ID` | The task id. Required. It must be `review.awaiting_human` (or resume a `review.integrating`/`review.ready_to_merge` task left by an earlier `merge`). |

Before it acts, `merge`:

1. fetches `[repository] remote` and requires `work/<task>` to still be `verified_sha`.
   When the branch moved, every verdict and the approval become invalid: the task returns to `review.verifying`, the reason is recorded, and `merge` exits `3`.
2. recomputes the verification digest from the task's verdicts and requires it to equal the recorded `verdict_set_digest`.
   A changed or new verdict for the same commit makes the approval invalid: the task returns to `review.verifying` with the reason recorded, and `merge` exits `3`.
   `radian arc run` then verifies it again.

Then it:

1. writes your `human` pass verdict for `verified_sha` (`runs/<task>/attempts/<attempt id>/verdicts/human.json`) and moves the task to `review.integrating`.
   The verdict has no `approver` for a `merge` you ran, and `approver` `auto` when `radian arc run` merged it at auto level `merge`.
2. runs the integration stage against the current remote target branch (`[integration] target_branch`, else `[repository] main_branch`). The candidate is built with `[integration] style` and the current `[integration] key_commands` run on it (see [Integration](integration.md)).
   A conflict, a failing key command, or a refused `rebase` moves the task to `review.rework_pending` with the reason, and `merge` exits `0`.
   A passing integration moves the task to `review.ready_to_merge`.
3. lands the candidate as [`integration.method`](config.md#configtoml) says: `push` pushes it to the remote target branch without force and integrates again when the target moved or the push was rejected, at most 3 times in one command; `command` runs the line and `pr` the built-in preset, and arc reads their last non-empty line (see [the command method](integration.md#the-command-method) and [the pull-request preset](integration.md#the-pull-request-preset)).
   A push that does not settle or a `pending` answer exits `3`, and a later `merge` continues.
4. checks that what landed is the candidate (see [The after-push check](integration.md#the-after-push-check) and [the command check](integration.md#the-command-check)). On a passing check it moves the task to `done.merged`, recording the `after_merge` hook and `merged_sha` in the same write. arc itself removes nothing (the branch stays on the remote until a hook deletes it); the default `[hooks] after_merge` lines do (see [Hooks](hooks.md#hooks)).

On `done.merged` it prints `<task> done.merged (<candidate sha>)`, followed by `<new task id> proposed (follow-ups from <plan id>)` when the merge left the plan with no open task and it has review items to collect (see [The review's follow-up](gate-review.md#the-reviews-follow-up)). A hook command that fails is a `warning:` line after it and does not undo the merge, and neither does a follow-up task that could not be added. It runs its pending hook unless a loop holds `runner.lock`, which runs it.

### The pull-request preset

`[integration] method = "pr"` is the built-in `command` method for a protected branch on GitHub.
It runs a fixed script in place of [`integration.command`](config.md#configtoml), through the same runner with the same environment and the same answer, check and `pending` handling (see [the command method](integration.md#the-command-method)); `[integration] command` with `method = "pr"` is a config error, as `command` with `method = "push"` is.
The script in full, its `--repo` rule, its `gh` requirement and the way to use another host are in [the pull-request preset](integration.md#the-pull-request-preset).

### Merge exit codes

| Code | Meaning | When |
|---|---|---|
| `0` | success | The task reached `done.merged`, or the integration sent it to `review.rework_pending`. |
| `1` | an error | A git or file error. |
| `3` | the state does not allow the operation | The state does not allow the merge, the branch moved, the digest changed (the task returns to `review.verifying`), 3 integration runs did not settle, or an `[integration] command` (or the `pr` preset) answered `pending`. |
| `4` | a lock is held by another process | Another process holds `state.lock`. |

## `radian arc reject <TASK> --note <TEXT>`

`reject` refuses a task that passed the gate, with a note for the lead.
It is the owner's action (there is no MCP tool for it).

```text
radian arc reject <TASK> --note <TEXT>
```

| Argument | Value | What it does |
|---|---|---|
| `<TASK>` | `ID` | The task id. Required. It must be `review.awaiting_human`. |
| `--note` | `TEXT` | Why the task is refused. Required; an empty note is a usage error. |

It writes the note to `runs/<task>/attempts/<attempt id>/reject.md` and moves the task to `review.rework_pending`.
`show` prints the note as the task's `rework_reason`; `list` and `show` are how the lead reads it.
It prints `<task> review.rework_pending`.
It exits `0` on success, `1` on an error, `2` for an empty note, `3` when the task is not `review.awaiting_human` (or changed outside arc), and `4` when another process holds `state.lock`.

