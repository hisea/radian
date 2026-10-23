---
description: The arc commands: every subcommand, the lead's MCP server, and the exit codes.
---

# arc

arc is a plan and task orchestrator built into the same `radian` binary.
It keeps a plan and its tasks as files in the repository, runs each task as a separate headless `radian` worker, and merges only what passes a fixed gate.

This build has the storage layer, the subcommands that read and write it: `init`, `add`, `list`, `show`, `adopt`, `confirm`, `auto`, `pause`, `start`, `reverify`, `cancel`, `reset-budget`, `merge`, `reject`, and `run`.
It also has `radian arc mcp`, the MCP server a lead writes plans and tasks through, the attempt runner behind `start`, and the gate and merge behind `start`, `merge` and `run`.
`radian arc run` is the scheduling loop, and a bare `radian arc` opens the read-only TUI (see [The arc TUI](#the-arc-tui)).

This page holds the `radian arc` commands; the gate and its failures are in [`arc-gate`](arc-gate.md), and the files arc keeps are in [`arc-files`](arc-files.md).

## `radian arc`

`radian arc` with no subcommand opens the read-only TUI; its pages and keys are in [The arc TUI](#the-arc-tui).
Without setup, `radian arc` and every subcommand except `init` refuse with a line that names `radian arc init` and exit `3`; without a terminal, `radian arc` exits `2`.

## `radian arc init [--track]`

`init` sets arc up for this repository.
It runs anywhere inside the working tree and works on the repository root.

It finds:

- the repository root (`git rev-parse --show-toplevel`);
- the `origin` URL (`git remote get-url origin`);
- the main branch: `origin`'s own HEAD when it is known, else the checked-out branch, else `main`.

It writes `config.toml` with those values, and it creates the layout below.
A second `init` keeps an existing `config.toml` byte for byte and creates nothing twice.

It adds the arc paths to `.git/info/exclude`, a local file: `init` never changes a committed `.gitignore`.
Each line is added once; a second run adds nothing.

| Line | Without `--track` | With `--track` |
|---|---|---|
| `/.radian/arc/config.toml` | excluded | tracked |
| `/.radian/arc/plans/` | excluded | tracked |
| `/.radian/arc/tasks/` | excluded | tracked |
| `/.radian/arc/ledger/` | excluded | excluded |
| `/.radian/arc/runs/` | excluded | excluded |
| `/.radian/arc/attempts/` | excluded | excluded |
| `/.radian/arc/gates/` | excluded | excluded |
| `/.radian/arc/state.lock` | excluded | excluded |
| `/.radian/arc/runner.lock` | excluded | excluded |
| `/.radian/arc/worktrees/` | excluded | excluded |

`--track` makes git see `config.toml`, `plans/`, and `tasks/`, so you can commit them when you want; arc never commits them.
Removing a line that an earlier run added is up to you: `init` only appends.

Then it checks its requirements and prints one line per check:

| Check | What it runs |
|---|---|
| `provider credential` | Reads the stored credential (`radian login` writes it to `~/.radian/credentials/auth.json`). No entry is a warning. |
| `` `origin` reachable `` | `git ls-remote origin`. |
| `` push to `origin` `<main>` `` | `git push --dry-run <main>:<main>`. A refusal here means the first real push will be refused too. |
| `workspace sandbox` | Builds a `workspace` sandbox over the repository: `sandbox-exec` on macOS, bubblewrap on Linux. |

A failed check is a warning line, not a refusal: `init` still exits `0`, because a missing credential or a slow remote is something you can fix after setup.

Finally it prints the command that registers the arc MCP server with a lead.
It does not change any configuration of its own:

```sh
claude mcp add arc -- radian arc mcp
```

`init` exits `0` when it wrote the layout and `1` when the directory is not a git working tree or the repository has no `origin` remote.

## `radian arc add`

`add` creates one plan or one task from standard input.
The new id is printed on stdout, one line.
Plans and tasks count in separate number sequences, so a plan and a task may carry the same number; the prefix tells them apart (see [Plan and task files](arc-files.md#plan-and-task-files)).

### `radian arc add plan --slug <SLUG> --title <TITLE>`

Reads the request text from stdin to end of input, writes `plans/<id>.md`, and prints the id.

| Flag | Value | What it does |
|---|---|---|
| `--slug` | `SLUG` | The part of the id after the prefix and number: 1 to 48 characters from `a-z`, `0-9`, `-`, with no leading or trailing `-`. Required. |
| `--title` | `TITLE` | The plan's title. Required. It may not be empty. |

The body is the request text, then a `## Change records` section.
A confirmed plan may only gain change records there; the request text itself is never rewritten.

### `radian arc add task --plan <ID> --slug <SLUG> --title <TITLE> [--after <ID>]... [--allow-noop] [--stage <NAME>]...`

Reads the brief from stdin to end of input, writes `tasks/<id>.md`, appends the id to the plan's `tasks` list, and prints the id.

| Flag | Value | What it does |
|---|---|---|
| `--plan` | `ID` | The plan the task belongs to. Required. |
| `--slug` | `SLUG` | The same slug rule as a plan. Required. |
| `--title` | `TITLE` | The task's title. Required. It may not be empty. |
| `--after` | `ID` | A task that must be merged first. Repeat the flag for several. |
| `--allow-noop` | — | The task may deliver no change on purpose. Off by default. |
| `--stage` | `NAME` | An extra gate stage to run for this task. Repeat the flag. The value is `commands`, `mutation`, `review`, `ci`, or `human`. |

A task can only be added to a plan whose `status` is `draft`; a confirmed plan exits `3`.
An `--after` id must name a task of the same plan or a task whose state is `done`; anything else exits `3`.

## `radian arc list [--plan <ID>] [--json]`

Lists plans, each with its tasks in the plan's order.
Each task line shows `state.phase`, the attempt number, and `retry at <UTC time>` when the task has a `retry_at`.

| Flag | Value | What it does |
|---|---|---|
| `--plan` | `ID` | Only this plan. |
| `--json` | — | Print one JSON array instead of lines. |

A plan or task whose file does not match arc's record is marked `(changed outside arc)`.

The JSON array holds one object per plan: `id`, `title`, `status`, `auto`, `max_cost_usd`, `max_wall_time_secs`, `changed_outside_arc`, and `tasks`.
Each task object holds `id`, `plan`, `title`, `state`, `phase`, `attempt`, `attempt_id`, `token`, `cost_usd`, `auto_rework`, `after`, `supersedes`, `allow_noop`, `extra_stages`, `max_attempts`, `max_cost_usd`, `max_wall_time_secs`, `stall_timeout_secs`, `base_sha`, `head_sha`, `verified_sha`, `verdict_set_digest`, `retry_at`, and `changed_outside_arc`.

## `radian arc show <ID> [--json]`

Prints one plan or task: every field, whether it changed outside arc, then its body.

The id names one plan or task. An id that names both a plan and a task is refused with exit `3`, naming both files (see [Plan and task files](arc-files.md#plan-and-task-files)).
`list --plan <id>` always reads the plan.

| Argument | Value | What it does |
|---|---|---|
| `<ID>` | a plan or task id | Required. An id that names both a plan and a task is refused (`3`). |

| Flag | Value | What it does |
|---|---|---|
| `--json` | — | Print one JSON object instead of fields. |

The object holds the same keys as in `list --json`, plus `body`, `rework_reason`, and the newest review verdict's `review`, `review_verdict`, `review_session_ids` and `review_cost_micros`.
An unset field is `null`; `rework_reason` is `null` unless the task is in `review.verifying` or `review.rework_pending` and has a recorded reason (a failed stage, a moved branch, a gate that left no record, a reject note, or the automatic rework limit that paused the plan).
`review` is `{"summary", "blocking", "suggestions", "verified"}` from the newest `review` verdict, or `null`; each item of `blocking` and `suggestions` is an object with `text` and, when it has them, `level`, `file`, `line` and `pre_existing` (see [the review stage](arc-gate.md#the-gate-run)); `review_verdict` is its path or `null`; `review_session_ids` lists every session the stage ran; `review_cost_micros` is their summed cost in micros of a US dollar (`null` when the run reported none).
For a task with a review verdict, the text form also prints `review_verdict`, `review_summary`, `review_blocking`, `review_suggestions`, `review_verified`, `review_session_ids` and `review_cost_micros`.
An id that names nothing exits `1`.

## `radian arc adopt <ID>`

Every file arc writes is recorded with a content hash in the ledger.
When a plan or task file on disk is not the one arc last wrote, arc does not act on it: the change may have come from an editor, a pull, or a checkout.

`adopt` records the current content as arc's own, so the item can be used again.
It accepts a change to the body, the title, `after`, `supersedes`, `allow_noop`, `extra_stages`, the per-task limits, and a plan's limits and `tasks` list.

| Argument | Value | What it does |
|---|---|---|
| `<ID>` | a plan or task id | Required. The item whose current file becomes arc's own. |

It refuses with exit `3` when the id names both a plan and a task (see [Plan and task files](arc-files.md#plan-and-task-files)), or when the outside change touched a field only arc (for a task) or the owner (for a plan) sets:

- a task: `state`, `phase`, `attempt`, `attempt_id`, `token`, `cost_usd`, `auto_rework`, `retry_at`, `base_sha`, `head_sha`, `verified_sha`, `verdict_set_digest`;
- a plan: `status`, `auto`.

A file that already matches the ledger is a no-op and prints so.
Without a recorded version to compare against, `adopt` refuses: it cannot tell which fields changed.

## `radian arc confirm <PLAN>`

`confirm` moves a plan from `draft` to `confirmed`.
A confirmed plan gets no new tasks, and its request text can only gain change records (see [Plan and task files](arc-files.md#plan-and-task-files)).
A plan's `auto` level may only be other than `off` once it is confirmed.

| Argument | Value | What it does |
|---|---|---|
| `<PLAN>` | `ID` | The plan id. Required. |

It refuses with exit `3` when the plan has no tasks, when it is already confirmed, when its file changed outside arc, and when the worker's `radian exec` line is rejected (see `start` and [`worker.exec_args`](arc-files.md#configtoml)).
A refusal writes nothing.

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
A value outside the set is a usage error (`2`).

### Automatic reworks

A failed stage or a failed integration sends a task to `review.rework_pending`.
While the task's plan has an `auto` level other than `off`, arc counts that rework in the task's `auto_rework` field.
When the count would go past `[limits] max_auto_rework`, arc pauses the whole plan: its `auto` becomes `off`, the task stays in `review.rework_pending`, and both `radian arc run` and `show` (as part of `rework_reason`) say why.
arc never resets a task's count.
A plan budget pausing the plan also sets `auto` to `off`; `run` starts and merges nothing for a plan at `off`.

## `radian arc pause <PLAN>`

`pause` sets the plan's `auto` level to `off`.

| Argument | Value | What it does |
|---|---|---|
| `<PLAN>` | `ID` | The plan id. Required. |

A task that is already started keeps its state; the next `radian arc run` reads the level before it starts or merges anything.
It exits `0` on a plan that is already `off`.

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

Before it reconciles, claims or verifies anything, `start` parses the line the worker would get: the wrapper's fixed `radian exec` arguments plus `[worker] exec_args` (see the `worker.exec_args` row of [`config.toml`](arc-files.md#configtoml)).
A line `radian exec` would reject, or one that repeats an argument the wrapper sets, refuses the command with exit `3` and the parser's own message, names `worker.exec_args`, claims nothing, and changes no file.

Before it claims anything it reconciles every task in `doing` (see [Reconcile](arc-gate.md#reconcile)) and moves a `todo.retry_wait` task whose `retry_at` has passed back to `todo.ready`.

A claim is refused with exit `3` and changes nothing when:

- the task is not `todo.ready`;
- its plan is not `confirmed`;
- an `after` task is not `done`;
- `[limits] concurrency` tasks are already in `doing`.

The claim is one transaction: the task's `attempt` goes up by one, `attempt_id` becomes `<task>-<attempt>`, a random `token` is written, and the task moves to `doing.starting`.
arc then runs the hidden `radian arc work` wrapper as a child, with the brief on its stdin; the wrapper moves the task to `doing.running`.

Before the worker starts, the wrapper runs the profile's `before_attempt` steps in the worktree (see the [`before_attempt` row](arc-files.md#configtoml)); a first attempt's steps do nothing.
`merge-main` fetches the remote main and merges it with `git merge --no-ff --no-edit` unless it is already an ancestor; a conflict leaves the worktree mid-merge and the worker's brief opens with the conflicted paths (see [A conflict with main](arc-gate.md#a-conflict-with-main)).
A failed fetch or merge ends the attempt with git's message.

The wrapper enforces the attempt's limits itself, so they hold even after the process that started the wrapper is gone:

- no new line in `runs/<task>/attempts/<attempt id>/result.jsonl` for the task's `stall_timeout_secs` (else `[limits] stall_timeout_secs`) sends SIGTERM to the worker's process group;
- `max_wall_time_secs` (else `[limits] max_wall_time_secs`) since the wrapper started does the same; this clock also bounds the wrapper's own git steps, so a fetch or a push still running at the limit is stopped the same way.
- after `[limits] cancel_grace_secs` the worker gets SIGKILL.

A limit of `0` means no limit for that timer.
The wrapper records the class in `runs/<task>/attempts/<attempt id>/signal.json` (`stall` or `wall_time`), forwards SIGTERM, waits for the worker, and still writes its `AttemptFinished` record.
`start` waits for the wrapper and applies the result; `radian arc run` does not wait (see `run`).

When the attempt ends, arc applies the result in one transaction (the [result table](arc-gate.md#failures-retries-and-the-budget)) and prints one line:

```text
<attempt id> <outcome> -> <state>.<phase>
<attempt id> <outcome> -> <state>.<phase> (retry at <UTC time>)
<attempt id> <outcome> -> <state>.<phase> (<reason>)
```

For example `TK0001-work-1 completed -> review.verifying`.
A result whose token is not the task's current attempt prints `<attempt id> ignored: <why>` and changes nothing.
Before its own line, `start` prints `reconciled: <line>` for each task the reconcile step changed.

A pushed `completed` attempt continues into the gate in the same process, so one `start` takes a task from `todo.ready` to `review.awaiting_human` or `review.rework_pending`.
The verification prints its own line:

```text
<attempt id> verify -> <state>.<phase>
<attempt id> verify -> <state>.<phase> (<stage>: <reason> (verdict <path>))
```

For example `TK0001-work-1 verify -> review.awaiting_human`, or `TK0001-work-1 verify -> review.rework_pending (commands: command_failed (verdict /…/verdicts/commands.json))`.
When verification cannot run (a moved branch, a stage this build does not have), the task stays in `review.verifying`, `start` prints the attempt's own line and then the reason on stderr, and exits `3`.

`start` exits `0` when the task left `doing`, `1` when it is still in `doing` after the attempt (or on an internal error), `3` when the claim is refused or verification refuses, and `4` when another process holds `state.lock`.
`start` waits for the wrapper, so it exits only after the attempt ends; `radian arc run` does not wait.

## `radian arc cancel <TASK>`

`cancel` stops one task.
It reconciles `doing` tasks first, as `start` does.

| Argument | Value | What it does |
|---|---|---|
| `<TASK>` | `ID` | The task id. Required. |

### What cancel does

| The task is | What `cancel` does |
|---|---|
| `todo` (any phase) | moves it to `failed.cancelled` |
| `doing`, starting or running | SIGTERM to the wrapper, then SIGKILL after `[limits] cancel_grace_secs`; when the wrapper's `AttemptFinished` arrives the task goes to `failed.cancelled`; when it never arrives the task goes to `doing.reconciling` |
| `doing.reconciling` | checks the wrapper's pid and start time; signals it if it is still that process; moves the task to `failed.cancelled` |
| `review`, `done`, `failed` | refused with exit `3` |

The wrapper's pid and its start time both come from the attempt record, so a pid the operating system reused is never signalled.
`cancel` prints `<task> <state>.<phase>`, for example `TK0001-work failed.cancelled`.
It exits `0` when it stopped the task or left it `doing.reconciling`, `3` for a refused state, and `4` when another process holds `state.lock`.

## `radian arc reset-budget <ID>`

`reset-budget` returns budget-paused tasks to `todo.ready` and resets the counted cost to zero from now on.
The id names one plan or task; an id that names both is refused with exit `3` (see [Plan and task files](arc-files.md#plan-and-task-files)).

| Argument | Value | What it does |
|---|---|---|
| `<ID>` | a plan or task id | Required. A plan id resets every paused task of that plan. |

- Task form: the task must be `todo.paused_budget`; it moves to `todo.ready` and its `cost_usd` becomes `0`. Any other state is refused (`3`).
- Plan form: every task of the plan that is `todo.paused_budget` moves to `todo.ready`; every task of the plan gets `cost_usd` `0`, because the plan's budget counts all of them. The plan's `auto` level is not changed.

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
| `<TASK>` | `ID` | The task id. Required. It must be `review.rework_pending` or `review.awaiting_human`. |

It applies only when the task's `rework_reason`, if there is one, comes from a verification stage (`commands`, `mutation`, `review`, or `ci`).
A reject note, a failed delivery check, a failed integration, and every other state are refused with exit `3`.
A plan paused by the automatic rework limit does not block `reverify` when the task's own reason is a failed verification stage.

A reverify claims a new attempt without a worker:

- `attempt` goes up by one, and `attempt_id` and `token` are new;
- `head_sha` and `base_sha` stay the previous attempt's, and nothing is pushed;
- the attempt result's class is `reverify`;
- the task moves to `review.verifying` and is verified at once in the same command, as `start` does after a pushed attempt.

The new attempt's verdicts are its own: an earlier attempt's verdict is not reused and no verdict is ever rewritten.
The remote `work/<task>` must still equal `head_sha`; when it moved, `reverify` exits `3` with the moved-branch reason and the task stays in `review.verifying`, as verification does.

A reverify attempt does not count toward `max_attempts`, costs nothing, and is not an automatic rework (`auto_rework` does not change).

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
| `3` | the state does not allow the operation | The task is in another state, its rework reason is not a verification stage, it has no `head_sha`, the branch moved, or a listed stage is not built in this build. |
| `4` | a lock is held by another process | Another process holds `state.lock`. |

## `radian arc merge <TASK>`

`merge` takes your approval, builds the integration candidate, pushes it to the remote main, and finishes the task.
It is the owner's action (there is no MCP tool for it).
At auto level `merge`, `radian arc run` takes the same approval and starts the integration in the background; a later pass reads its verdict and pushes.

```text
radian arc merge <TASK>
```

| Argument | Value | What it does |
|---|---|---|
| `<TASK>` | `ID` | The task id. Required. It must be `review.awaiting_human` (or resume a `review.integrating`/`review.ready_to_merge` task left by an earlier `merge`). |

Before it acts, `merge`:

1. fetches `[repository] remote` and requires `work/<task>` to still be `verified_sha`.
   When the branch moved, every verdict and the approval of the old commit become invalid: the task returns to `review.verifying`, the reason is recorded, and `merge` exits `3`.
2. recomputes the verification digest from the task's verdicts and requires it to equal the recorded `verdict_set_digest`.
   A changed or new verdict for the same commit makes the approval invalid: the task returns to `review.verifying` with the reason recorded, and `merge` exits `3`.
   `radian arc run` then verifies it again.

Then it:

1. writes your `human` pass verdict for `verified_sha` (`runs/<task>/attempts/<attempt id>/verdicts/human.json`) and moves the task to `review.integrating`.
   The verdict has no `approver` when you ran `merge`, and `approver` `auto` when `radian arc run` merged it at auto level `merge`.
2. runs the integration stage against the current remote main. The candidate is `head_sha` when main is still the task's `base_sha`; otherwise it is a merge commit of `head_sha` onto main (arc never rebases). The profile's `key_commands` run on the candidate.
   A conflict or a failing key command moves the task to `review.rework_pending` with the reason, and `merge` exits `0`.
   A passing integration moves the task to `review.ready_to_merge`.
3. pushes the candidate to the remote main without force: `git push <remote> <candidate>:refs/heads/<main>`.
   When main moved after integration (or the push is rejected as non-fast-forward), the task returns to `review.integrating` and `merge` integrates again, at most 3 times in one command. After that it exits `3` with the task in `review.integrating`; a later `merge` continues from there.
4. checks with `git ls-remote` that the remote main is the candidate and that the candidate contains `head_sha`, moves the task to `done.merged`, and removes the task's worktree (`.radian/arc/worktrees/<task>`).
   The task's branch stays on the remote.

`merge` never checks out or updates the main checkout's working tree: it fetches, pushes, and reads refs only.
On `done.merged` it prints `<task> done.merged (<candidate sha>)`; a failed worktree removal is a `warning:` line after it and does not undo the merge.

The exit codes are the command's own:

### Merge exit codes

| Code | Meaning | When |
|---|---|---|
| `0` | success | The task reached `done.merged`, or the integration sent it to `review.rework_pending`. |
| `1` | an error | A git or file error. |
| `3` | the state does not allow the operation | The state does not allow the merge, the branch moved, the digest changed (the task returns to `review.verifying`), or 3 integration runs did not settle (the task stays in `review.integrating`). |
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

## `radian arc run`

`run` is the scheduling loop without an interface.
It takes `runner.lock`, makes every transition the auto levels allow now, saves the state, and exits.
A pass only schedules: it starts a worker or a gate, collects what one left, and moves the state, so a pass ends in seconds.
Waiting for you, a `retry_at` in the future, a running worker, or a running gate does not keep it running.
A task of a plan at `auto` `off` is never started or merged by `run`; its verification still advances.

Before its first pass, and before each pass claims a task, it checks the worker's `radian exec` line the way `start` does: a rejected line exits `3`, claims nothing, and changes no file (see [`worker.exec_args`](arc-files.md#configtoml)).

| Flag | Value | Default | What it does |
|---|---|---|---|
| `--watch` | — | off | Repeat the loop: after a pass that ends quiet, wait `--interval` seconds and run another pass, until SIGINT or SIGTERM. Holds `runner.lock` the whole time. |
| `--interval` | `SECONDS` | `30` | Whole seconds between passes, 5 to 3600. Only with `--watch`: without it, or with a value outside 5 to 3600, the command is a usage error (`2`). |

One pass repeats until a pass changes nothing, in this order:

1. reconcile every task in `doing` (see [Reconcile](arc-gate.md#reconcile)): apply an `AttemptFinished` that is there, leave a live attempt alone, apply the retry rules to a dead one;
2. move every `todo.retry_wait` task whose `retry_at` has passed back to `todo.ready`;
3. for every task in `review.verifying` (every plan, including a task that a moved branch or a changed digest sent back): apply the verdicts a background gate left, or start its gate in the background;
4. for each confirmed plan at `auto` `run` or `merge`, claim every `todo.ready` task whose `after` tasks are merged, up to `[limits] concurrency` attempts in flight, and start each wrapper without waiting for it;
5. for each plan at `auto` `merge`, approve a verified task and start its integration in the background, or finish an integration a gate left.
   `run` writes the same `human` pass verdict `radian arc merge` writes, with `approver` `auto` in it.

The long work runs outside the pass: a worker is a separate `radian arc work` process with its own limits and record, and a gate is a separate process with its own record.
A stage this build does not run, or a branch that moved, is reported in the summary; the gate's refusal is recorded, and `run` starts no new gate for that attempt until the inputs change (the branch comes back, `config.toml` changes, or `start`/`reverify` claims a new attempt).
A gate whose process exited without writing any record is reported as `<task> verify cannot run: its gate exited without a record` (or `<task> integration cannot run: …`), recorded for that attempt; no new gate starts while its inputs stay the same, and `show` prints the same text as the `rework_reason` (see [the gate run](arc-gate.md#the-gate-run)).
The next pass applies a wrapper's early failure (a refused argument, a held worktree lock) the same way, so its concurrency slot is free at once (see [Reconcile](arc-gate.md#reconcile)).

A one-shot `run` starts its work and exits without waiting for it; the next `run` collects the result.
Use `--watch` (or the arc interface, when it exists) to keep advancing.

`run` writes one line per transition on stdout, in order, and flushes it when the loop makes the transition; it does not wait for the command to end.
A started worker prints `<attempt id> started -> doing.running`; a started gate prints `<attempt id> verify started (pid <pid>)`, an approval prints `<task> approved -> review.integrating (<attempt id>)`, an integration prints `<task> integration started (pid <pid>)`, and a conflict rework prints `<task> conflict with main -> todo.ready (automatic rework)`.
When a pass changes nothing, a summary of what waits on whom ends the pass:

```text
TK0001-a-1 started -> doing.running
summary
PL0001-plan (auto run)
  TK0001-a doing.running: running its attempt
  TK0002-b todo.ready: waiting for a free concurrency slot
```

A pass that wrote no transition line starts its summary with `no transition`.
A file that changed outside arc is skipped and named in the summary; it does not stop the loop.
A plan whose budget or automatic-rework limit paused it is named with its reason.

### `--watch`

`radian arc run --watch` repeats the pass. After every pass (which always ends quiet) it waits `--interval` seconds, then runs another pass; it holds `runner.lock` for the whole loop, so a second `run` exits `4` as usual.
It is a foreground loop that you start and stop, not a daemon: while it is not running, nothing advances.

With `--watch`, the summary is printed only when it differs from the summary the loop printed last, so a quiet loop does not repeat the same lines every interval. Every transition line is still printed when it happens.

A pass that returns an error prints one `radian arc: <message>` line and the loop waits its interval and runs the next pass; the lock and every state file stay as they are.
A one-shot `run` prints the same line and exits `1` instead.

On SIGINT or SIGTERM the loop starts no new worker and no new gate.
A Ctrl-C in the terminal reaches the loop only: each worker and each gate runs in its own process group, and both treat SIGINT exactly as SIGTERM.
A running worker is left alone: it is a separate process with its own limits and record, and the next pass (or the next `run`) collects it.
A running gate is signalled and gets `[limits] cancel_grace_secs` before SIGKILL; the gate forwards the signal to the stage it is running (the `commands` shell or the review session), waits for it, and exits without a record; a gate that did not finish is started again by the next loop as unverified, and the verdicts it already wrote are reused, unless it exited without writing any record at all: then the next pass reports it and starts no gate while its inputs are unchanged (see [the gate run](arc-gate.md#the-gate-run)).
The loop then prints the summary, releases `runner.lock`, and exits `0`.

It exits `0` when it ran to quiet, even when a task failed, and `0` on SIGINT or SIGTERM with `--watch`; `1` on a git or file error, `3` when the worker's `radian exec` line is rejected, and `4` when another process holds `runner.lock`. It never waits for the lock.

## `radian arc mcp`

`mcp` serves the lead's tools over standard input and output, and exits `0` when its client closes the connection.
A client that closes before the handshake finishes exits `1` and says which handshake request was cut off.
Like every subcommand except `init` it refuses without setup (exit `3`) and names `radian arc init`.

The server speaks MCP over stdio and is meant to be started by a lead, not by hand.
It reads the repository that contains its working directory.
Set `cwd` so it works whatever directory the session started in: in a project file at the repository root, `"cwd": "."` is that root (a relative `cwd` resolves against the file that declares it).
Without `cwd`, register it from inside the repository, or give that directory as its working directory.

Register it with Claude Code:

```sh
claude mcp add arc -- radian arc mcp
```

Or add it to a project `.mcp.json`, in the shared form every MCP client reads:

```json
{
  "mcpServers": {
    "arc": {"command": "radian", "args": ["arc", "mcp"], "cwd": "."}
  }
}
```

Radian's own form is the same server with an `id`:

```json
{
  "servers": [
    {"id": "arc", "command": "radian", "args": ["arc", "mcp"], "cwd": "."}
  ]
}
```

See `mcp` for both forms and how a project file is trusted.
A tool from a project `.mcp.json` asks before running under the `workspace` tier, like any other MCP tool.

The server takes only `state.lock`, never `runner.lock`, so the lead can add tasks while a scheduling loop runs.
Every tool is one of the store's transactions: a plan or task file that changed outside arc is refused with that reason, and the tool call fails without writing anything.

### The tools

| Tool | Parameters | Result |
|---|---|---|
| `add_plan` | `slug` (string, required), `title` (string, required), `request` (string, required) | `{"id": "<plan id>"}` |
| `add_task` | `plan` (string, required), `slug` (string, required), `title` (string, required), `brief` (string, required), `after` (list of ids, optional), `allow_noop` (boolean, optional, default `false`), `extra_stages` (list of stage names, optional) | `{"id": "<task id>"}` |
| `update_plan` | `plan` (string, required), `title` (string, optional), `request` (string, optional), `change_record` (string, optional) | the plan, as `radian arc show <plan> --json` |
| `add_rework` | `task` (string, required), `brief` (string, required) | the task, as `radian arc show <task> --json` |
| `list` | `plan` (string, optional: only this plan) | the JSON array `radian arc list --json` prints |
| `show` | `id` (string, required) | the JSON object `radian arc show --json` prints |

There is no tool to confirm a plan, start a task, merge, reject, change `auto`, or approve: those are yours (see `confirm`, `auto`, and `pause` above).

`add_plan` creates a `draft` plan with `auto` `off`.
The `slug` rule and the title rule are the ones on this page; an empty title is refused.

`add_task` keeps the rules of `radian arc add task`: the plan must be `draft`, each `after` id must name a task of the same plan or a task whose state is `done`, and each `extra_stages` name must be a gate stage.

`update_plan` depends on the plan's state:

- on a `draft` plan, `title` and `request` replace the plan's; a `change_record` is refused (`3`);
- on a `confirmed` plan, `title` and `request` are refused (`3`) because the request text is never rewritten, and `change_record` is required: it is appended under the plan's `## Change records` heading with the UTC time, and the plan returns to `draft` with `auto` `off`, keeping its tasks' state. New tasks can be added again.

A call with no field at all is refused: it would change nothing.

`add_rework` applies only to a task whose state is `review` (any phase); anything else is refused (`3`).
The brief is appended under `## Rework <n>`, where `n` is the task's `attempt` plus one, and the task moves to `todo.ready`.
It does not change `attempt`: the attempt number belongs to the claim that starts the next run.

### Tool errors

A tool that cannot do its work answers with `isError: true` and one text line: the reason.
That line is the same text the CLI prints for the same refusal, so a plan or task that changed outside arc says `changed outside arc` and names `radian arc adopt`.
An argument of the wrong type, a missing required argument, and an argument the tool does not take are errors that name the argument.
The arc exit codes below belong to the CLI; a tool error carries no code.
An id that names both a plan and a task is a tool error that names both files (see [Plan and task files](arc-files.md#plan-and-task-files)); the CLI exits `3` for it.

## Exit codes

Every subcommand of this page uses these codes.

| Code | Meaning |
|---|---|
| `0` | success |
| `1` | an error |
| `2` | a usage error |
| `3` | the state does not allow the operation (this includes a file changed outside arc, and arc not being set up) |
| `4` | a lock is held by another process |

Code `2` covers a bad flag or an argument value outside a closed set; a slug, an id, or a stage name that the rules refuse is also `2`.
A closed output pipe (`radian arc list | head`) is a normal stop: exit `0`.

## The arc TUI

`radian arc` opens a read-only view of plans, tasks, verdicts and journals; it writes no arc state and probes `runner.lock` (never creating it) for `● running` / `◻ stopped`. Header: `arc`, tabs `plans · task · merge` (`merge` disabled), `main <sha>` and loop state.

Plan marks: `✓` (merged, green), `●` (confirmed, accent), `◻` (draft, weak), then id and `auto <level> · <merged>/<total> · <cost>` (`draft · ` first for a draft; `—` when no cost). Frame: `<id> <status> · auto <level> · <n>/<m> merged · <cost>`, then title, tasks: a task after a plan task sits under its first parent (`├─`/`└─`), other parents are `also after <ids>`, roots in plan order, each task once. A row: mark, id, `state.phase` (mark's color), `#attempt`, cost, and `waits <ids>` for an unmerged-parent todo. Marks: `✓` done.merged green, `×` failed red, `●` doing/verifying accent, `!` awaiting_human/rework_pending accent bold (you or the lead acts), `◻` todo weak. Keys: `↑`/`↓` move in the focused column (the plan list, or the task list after `→`), `→`/`←` focus the task list/plan list, `Tab`/`Enter` open the task page (`Tab` on it goes back), `g` toggles the flat list (`after <ids>`), `Esc`/`q`/`Ctrl-C` quit.

Task frame: `<id> <state.phase> · attempt <N> · <cost> · plan <id>`, the title, `conversation · diff · attempts` and `attempt 1 2 3` (latest by default; `[`/`]` switch). Conversation: `~/.radian/sessions/<id>.jsonl` through the attach view (one weak line if none); `j`/`k` scroll, `f` follows. Diff: the merge commit's first-parent diff when merged, else `base_sha..head_sha`; files show `+N`/`-N`, the selected file's diff is a frame titled with its path, `n`/`p` file, `j`/`k` scroll, `f` end. Attempts: each attempt's verdicts (`✓ commands`, `× integration (conflict)`). `h`/`l` (or `←`/`→`) change tab, `[`/`]` attempt, `Tab`/`Esc` back, `q`/`Ctrl-C` quit.

Re-reads the store every 2 s; git runs in worker threads, a frame never waits (`loading…`).

## Not built yet

The gate stages `mutation` and `ci` are not built: verification refuses a profile or task that lists one, and the task stays in `review.verifying`.
The arc TUI's `merge` page and the owner's actions are not built; the TUI is not a scheduling loop (only `radian arc run` schedules) and holds no `runner.lock`.

## See also

- `cli` for every other `radian` command.
- `environment` for the files radian reads, including this one.