---
description: The arc commands: every subcommand, the lead's MCP server, and the exit codes.
---

# arc

arc is a plan and task orchestrator built into the same `radian` binary.
It keeps a plan and its tasks as files in the repository, runs each task as a separate headless `radian` worker, and merges only what passes a fixed gate.

This build has the storage layer, the subcommands that read and write it: `init`, `add`, `list`, `show`, `adopt`, `confirm`, `auto`, `priority`, `pause`, `start`, `reverify`, `cancel`, `reset-budget`, `merge`, `reject`, and `run`.
It also has `radian arc mcp`, the MCP server a lead writes plans and tasks through, the attempt runner behind `start`, and the gate and merge behind `start`, `merge` and `run`.
`radian arc run` is the scheduling loop, and a bare `radian arc` opens the TUI, where you see the plans and tasks and act on them (see [`arc-tui`](arc-tui.md)).

This page holds the `radian arc` commands; the gate and its failures are in [`arc-gate`](arc-gate.md), the lead's MCP server in [`arc-mcp`](arc-mcp.md), the TUI in [`arc-tui`](arc-tui.md), and the files arc keeps are in [`arc-files`](arc-files.md).

## `radian arc`

`radian arc` with no subcommand opens the TUI; its pages and keys are in [`arc-tui`](arc-tui.md).
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

A task can only be added to a plan whose `status` is `draft`; a confirmed or cancelled plan exits `3`.
An `--after` id must name a task of the same plan or a task whose state is `done`; anything else exits `3`.

## `radian arc list [--plan <ID>] [--all] [--json]`

Lists plans, each with its tasks in the plan's order.
Plans print in claim order, the order `radian arc run` serves them in: the higher `priority` first (`P0` before `P1`), then the plan id from small to large.
Each plan's header line shows its id, its `status`, its `auto` level, its `priority`, its progress as `<merged>/<tasks> merged`, its `total_cost_usd` (the sum of its tasks' totals), and its title.
Each task line shows `state.phase`, the attempt number, and `retry at <UTC time>` when the task has a `retry_at`.
A task's `cost_usd` is what the budget counts (every worker attempt and every `review` stage, reset by `reset-budget`); its `total_cost_usd` also includes every review and is never reset.

By default the finished work is left out, so the work in progress is what you see:

- every task in `done.merged` is left out; a plan that is shown still counts those tasks in its `<merged>/<tasks> merged` header;
- a plan whose tasks are all `done.merged` (a finished plan) is left out too; a plan with no tasks is always shown;
- a `cancelled` plan is left out; `--all` shows it.

Every other state is shown, `failed.*` included: a failed task needs attention.

When the default left something out, the last line counts it:

```text
3 finished plans and 12 merged tasks hidden (--all to show)
```

The count covers every plan and every merged task that was not printed. A part that is `0` is left out of the line, and a count of `1` is singular: `1 finished plan and 1 merged task hidden (--all to show)`, `2 cancelled plans and 3 merged tasks hidden (--all to show)`, or `2 merged tasks hidden (--all to show)` when no whole plan was left out.
When everything is hidden, that line is the whole output. Nothing hidden means no such line.

| Flag | Value | What it does |
|---|---|---|
| `--plan` | `ID` | Only this plan. It follows the default filter: its merged tasks are hidden unless `--all`, and a finished or cancelled plan is still printed with its header line, so you see that it exists. |
| `--all` | — | No filter: print every plan and every task, `done.merged` history and cancelled plans included. It takes no value. |
| `--json` | — | Print one JSON array instead of lines. Without `--all` it follows the same filter: a cancelled plan is left out, and a plan named with `--plan` is always included. |

A plan or task whose file does not match arc's record is marked `(changed outside arc)`.

The JSON array holds one object per plan: `id`, `title`, `status`, `auto`, `priority`, `max_cost_usd`, `max_wall_time_secs`, `cost_usd` (its tasks' `cost_usd` sum, the number the plan budget compares), `total_cost_usd` (its tasks' totals sum), `changed_outside_arc`, and `tasks`.
Each task object holds `id`, `plan`, `title`, `state`, `phase`, `attempt`, `attempt_id`, `token`, `cost_usd`, `total_cost_usd`, `auto_rework`, `retry_count`, `after`, `supersedes`, `allow_noop`, `extra_stages`, `max_attempts`, `max_cost_usd`, `max_wall_time_secs`, `stall_timeout_secs`, `base_sha`, `head_sha`, `verified_sha`, `verdict_set_digest`, `retry_at`, and `changed_outside_arc`.

## `radian arc show <ID> [--json]`

Prints one plan or task: every field, whether it changed outside arc, then its body.

A plan prints `cost_usd` and `total_cost_usd` beside its other fields: the first is the sum of its tasks' `cost_usd` and the number its budget compares, the second is the sum of their totals. Both are computed when the plan is read and are not stored in the plan file. A task prints both of its own numbers.

The id names one plan or task. An id that names both a plan and a task is refused with exit `3`, naming both files (see [Plan and task files](arc-files.md#plan-and-task-files)).
`list --plan <id>` always reads the plan.

| Argument | Value | What it does |
|---|---|---|
| `<ID>` | a plan or task id | Required. An id that names both a plan and a task is refused (`3`). |

| Flag | Value | What it does |
|---|---|---|
| `--json` | — | Print one JSON object instead of fields. |

The object holds the same keys as in `list --json`, plus `body`, `session_id`, `rework_reason`, and the newest review verdict's `review`, `review_verdict`, `review_session_ids`, `review_cost_micros` and `review_settings`.
An unset field is `null`; `session_id` is the current attempt's worker session id from its `execution.json`, and `null` when that attempt's wrapper wrote no record; `rework_reason` is `null` unless the task is in `review.verifying` or `review.rework_pending` and has a recorded reason (a failed stage, a moved branch, a gate that left no record, a gate that died after it started, a reject note, the automatic rework limit that paused the plan, a conflict that reached `limits.max_conflict_rework`, or an automatic reverify count that is used up).
`review` is `{"summary", "blocking", "suggestions", "verified"}` from the newest `review` verdict, or `null`; each item of `blocking` and `suggestions` is an object with `text` and, when it has them, `level`, `file`, `line` and `pre_existing` (see [the review stage](arc-gate.md#the-gate-run)); `review_verdict` is its path or `null`; `review_session_ids` lists every session the stage ran; `review_cost_micros` is their summed cost in micros of a US dollar (`null` when the run reported none); `review_settings` is `{"round", "exec_args", "timeout_secs", "timeout"}` (the round `first` or `rework`, the config key the session's arguments came from, its wall-time limit in seconds and the key that limit came from), or `null`.
For a task with a review verdict, the text form also prints `review_verdict`, `review_summary`, `review_blocking`, `review_suggestions`, `review_verified`, `review_session_ids`, `review_cost_micros`, and `review_round`, `review_exec_args`, `review_timeout_secs` and `review_timeout`.
The text form prints `session_id` after `attempt_id`.
An id that names nothing exits `1`.

### The `after` chain

A task's `after` lists the tasks that must be merged before it is claimed (see [Plan and task files](arc-files.md#plan-and-task-files)).
`show <task>` prints two more fields after `after`:

- `waits_on`: every task in the `after` chain, transitively, nearest first, each as `<id> <state>.<phase>`; `-` when it waits on nothing.
- `blocks`: every other task whose `after` chain contains this task, transitively, nearest first, in the same form; `-` when no task waits on it.

The walk is breadth-first over `after` and visits each task once.
An id that names no task is printed as `<id> missing`; only a hand-edited file that was adopted can hold one, because `add task` and the MCP `add_task` tool refuse an `after` id that names no task.

A task in `waits_on` that can never become `done.merged` stops the chain for good.
That is a `failed.*` task, a task whose plan is `cancelled` and that is not merged, or an id that names no task.
`show` prints the first such task as `never_runs: <id> <state>.<phase>` (or `never_runs: <id> missing`).
An `after` cycle, possible only through an adopted hand edit, is printed once as `cycle: <id> -> … -> <id>`, and the walk stops there instead of looping.
A plan's `show` has none of these fields.

The JSON object of a task holds `waits_on` and `blocks` too: arrays of `{"id": string, "state": string|null, "phase": string|null}`, nearest first, with `null` state and phase for an id that names no task.
It also holds `never_runs` (that object, or `null`) and `cycle` (an array of ids that starts and ends with the same id, or `null`).
The MCP `show` tool returns the same object.

## `radian arc adopt <ID>`

Every file arc writes is recorded with a content hash in the ledger.
When a plan or task file on disk is not the one arc last wrote, arc does not act on it: the change may have come from an editor, a pull, or a checkout.

`adopt` records the current content as arc's own, so the item can be used again.
It accepts a change to the body, the title, `after`, `supersedes`, `allow_noop`, `extra_stages`, the per-task limits, and a plan's limits and `tasks` list.

| Argument | Value | What it does |
|---|---|---|
| `<ID>` | a plan or task id | Required. The item whose current file becomes arc's own. |

It refuses with exit `3` when the id names both a plan and a task (see [Plan and task files](arc-files.md#plan-and-task-files)), or when the outside change touched a field only arc (for a task) or the owner (for a plan) sets:

- a task: `state`, `phase`, `attempt`, `attempt_id`, `token`, `cost_usd`, `total_cost_usd`, `auto_rework`, `retry_count`, `retry_at`, `base_sha`, `head_sha`, `verified_sha`, `verdict_set_digest`;
- a plan: `status`, `auto`, `priority`.

A file that already matches the ledger is a no-op and prints so.
Without a recorded version to compare against, `adopt` refuses: it cannot tell which fields changed.

## `radian arc confirm <ID>`

`confirm` is your action: it accepts a draft plan, or a task arc or the lead proposed.

A plan moves from `draft` to `confirmed`.
A confirmed plan gets no new `todo.ready` tasks, and its request text can only gain change records (see [Plan and task files](arc-files.md#plan-and-task-files)).
A plan's `auto` level may only be other than `off` once it is confirmed.

A task moves from `todo.proposed` to `todo.ready`.
The plan's `status` and `auto` do not change, and the plan does not return to `draft`.
A task waits in `todo.proposed` until you confirm it when arc proposed it from a review (see [The review's follow-up](arc-gate.md#the-reviews-follow-up)) or when the lead added it to a confirmed plan with the MCP `add_task` tool; the scheduling loop passes over it, and `start` refuses it.
A proposed task you do not want is ended with `radian arc cancel` (`failed.cancelled`).

| Argument | Value | What it does |
|---|---|---|
| `<ID>` | a plan id, or a task id | Required. An id that names both a plan and a task is refused (`3`), naming both files. |

A plan confirm refuses with exit `3` when the plan has no tasks, when it is already confirmed, when it is cancelled, when its file changed outside arc, and when the worker's `radian exec` line is rejected (see `start` and [`worker.exec_args`](arc-files.md#configtoml)).
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
- one `<id> waits on: <chain>` line per task the confirm would make `todo.ready` — the task itself, or a plan's `todo.ready` tasks in the plan's order — with the same entries as `show`'s [`waits_on`](#the-after-chain), or `<id> waits on: nothing`;
- a `never runs: <id> <state>.<phase>` line for each such task whose chain cannot finish (the same test as `show`'s `never_runs`), and a `cycle: <id> -> … -> <id>` line for a cycle;
- one `<id> queue: <n> of <m> todo.ready` line per such task: its place among every `todo.ready` task plus itself, in claim order (the plan's `priority`, the plan id, the task's order in the plan).

When the next pass would not claim the task, the queue line ends with the reason in parentheses, in the words `radian arc run`'s summary uses: `(waiting for <id> to merge)`, or ``(waiting for you to start it (`auto` is `off`))``.
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
`priority` only decides which plan gets the next free `[limits] concurrency` slot; it never preempts an attempt that is already running, so a change takes effect at the next pass that has a free slot (see [`run`](#radian-arc-run)).
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

`start` also claims the task you name when it is `todo.retry_wait`, even while its `retry_at` is still in the future: your `start` is a run-now, and the backoff exists for the scheduling loop. The claim is the same transaction as a `todo.ready` one, with the same concurrency check and the same printed line, and it clears `retry_at`.

A claim is refused with exit `3` and changes nothing when:

- the task is not `todo.ready` or `todo.retry_wait`; a `todo.proposed` task names its phase and waits for `radian arc confirm`, and `todo.paused_budget` and every `failed.*` phase are refused;
- its plan is not `confirmed`; a `cancelled` plan is refused, naming its status;
- an `after` task is not `done`;
- `[limits] concurrency` tasks are already in `doing`.

The claim is one transaction: the task's `attempt` goes up by one, `attempt_id` becomes `<task>-<attempt>`, a random `token` is written, and the task moves to `doing.starting`.
arc then runs the hidden `radian arc work` wrapper as a child, with the brief on its stdin; the wrapper moves the task to `doing.running`.

The worker runs in a session of its own: with [`worker.rework_session`](arc-files.md#configtoml) at `fresh`, which is the default, each attempt gets the session `<task>-<attempt>`.
With `resume` the task has one session instead, the task id, and every attempt after the first continues it.
The attempt's session id is in its `execution.json`, and `show` prints it as `session_id`.
A fresh attempt after the first cannot see the earlier conversation, so its stdin opens with a hand-off section, `## Where the previous attempt left off`: the newest earlier attempt's final report (cut at 8,000 characters, with a note when it is cut; a `reverify` attempt runs no worker and writes none, so the section walks back to the attempt before it and names it), the `git log --oneline` of the task's own commits, and the `git diff --stat` of the task's own change, both from the merge base of the remote main and the branch tip.
The section says the full history is on the branch and to read the files rather than assume.
The brief follows it, and a `merge-main` conflict section still comes first.

Before the worker starts, the wrapper runs the profile's `before_attempt` steps in the worktree (see the [`before_attempt` row](arc-files.md#configtoml)); a first attempt's steps do nothing.
`merge-main` fetches the remote main and merges it with `git merge --no-ff --no-edit` unless it is already an ancestor; a conflict leaves the worktree mid-merge and the worker's brief opens with the conflicted paths (see [A conflict with main](arc-gate.md#a-conflict-with-main)).
A failed fetch or merge ends the attempt with git's message.

The wrapper enforces the attempt's limits itself, so they hold even after the process that started the wrapper is gone:

- no new line in `runs/<task>/attempts/<attempt id>/result.jsonl` for the task's `stall_timeout_secs` (else `[limits] stall_timeout_secs`) sends SIGTERM to the worker's process group;
- `max_wall_time_secs` (else `[limits] max_wall_time_secs`) since the wrapper started does the same; this clock also bounds the wrapper's own git steps, so a fetch or a push still running at the limit is stopped the same way.
- a fetch, a push or an `ls-remote` is also bounded by [`limits.git_remote_timeout_secs`](arc-files.md#configtoml); whichever of it and the attempt's remaining wall time is shorter applies, and the call, with everything it started, is killed when it runs out.
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

A pushed `completed` attempt continues into the gate in the same process, so one `start` takes a task from `todo.ready` (or `todo.retry_wait`) to `review.awaiting_human` or `review.rework_pending`.
The verification prints its own line:

```text
<attempt id> verify -> <state>.<phase>
<attempt id> verify -> <state>.<phase> (<stage>: <reason> (verdict <path>))
```

For example `TK0001-work-1 verify -> review.awaiting_human`, or `TK0001-work-1 verify -> review.rework_pending (commands: command_failed (verdict /…/verdicts/commands.json))`.
When verification cannot run (a moved branch, a stage this build does not have), the task stays in `review.verifying`, `start` prints the attempt's own line and then the reason on stderr, and exits `3`.

`start` exits `0` when the task left `doing`, `1` when it is still in `doing` after the attempt (or on an internal error), `3` when the claim is refused or verification refuses, and `4` when another process holds `state.lock`.
`start` waits for the wrapper, so it exits only after the attempt ends; `radian arc run` does not wait.

## `radian arc cancel <ID>`

`cancel` stops one task, or ends every open task of a plan and the plan itself.
It is your action (there is no MCP tool for it), and it reconciles `doing` tasks first, as `start` does.

| Argument | Value | What it does |
|---|---|---|
| `<ID>` | a plan or task id | Required. An id that names both a plan and a task is refused (`3`), naming both files; an id that names neither exits `1`. |

### Task form

| The task is | What `cancel` does |
|---|---|
| `todo` (any phase) | moves it to `failed.cancelled` |
| `doing`, starting or running | SIGTERM to the wrapper, then SIGKILL after `[limits] cancel_grace_secs`; when the wrapper's `AttemptFinished` arrives the task goes to `failed.cancelled`; when it never arrives the task goes to `doing.reconciling` |
| `doing.reconciling` | checks the wrapper's pid and start time; signals it if it is still that process; moves the task to `failed.cancelled` |
| `review` (any phase) | stops a gate that is running for the task, then moves the task to `failed.cancelled` |
| `done`, `failed` | refused with exit `3` |

When a gate is running for a `review` task (its `runs/<task>/gate.json` record is `started` or `running`), `cancel` signals the gate process the same way a scheduling-loop stop does: SIGTERM, `[limits] cancel_grace_secs`, then SIGKILL, and the gate's record becomes `interrupted`.
The task moves only after the gate is stopped.

The wrapper's pid and its start time both come from the attempt record, so a pid the operating system reused is never signalled.
`cancel` prints `<task> <state>.<phase>`, for example `TK0001-work failed.cancelled`.

### Plan form

A plan id runs these steps in this order:

1. the plan's `auto` becomes `off`;
2. every task of the plan in `todo`, `doing` or `review` is cancelled by the task form's rules;
3. a task in `done` or `failed` is left as it is;
4. the plan's `status` becomes `cancelled`.

A task a gate is running for is stopped as the task form stops it.
If a task's cancel leaves it `doing.reconciling`, the plan still becomes `cancelled`; the next `radian arc run` pass ends the task at `failed.cancelled`, after the same process check the task form makes (see [Reconcile](arc-gate.md#reconcile)).
A plan that is already `cancelled` is refused with exit `3`.

`cancel` prints one `<task> <state>.<phase>` line for each task it changed, in the plan's task order, then `<plan> cancelled`:

```text
TK0002-b failed.cancelled
TK0003-c failed.cancelled
PL0001-plan cancelled
```

A cancelled plan is final.
`confirm`, `auto`, `priority`, `start`, `reverify`, `reset-budget`, `add task`, and the MCP `add_task`, `update_plan` and `add_rework` tools refuse it with exit `3` (the MCP tools: an error naming the status).
`radian arc run` never claims, gates or merges its tasks, and the summary does not list it as waiting.
`list` hides it unless `--all`, and `show <plan>` and `show <task>` still read it and its tasks.

`cancel` exits `0` when it stopped the task or plan or left a task `doing.reconciling`, `1` when the id names nothing, `3` for a refused state or an id that names both a plan and a task, and `4` when another process holds `state.lock`.

## `radian arc reset-budget <ID>`

`reset-budget` returns budget-paused tasks to `todo.ready` and resets the counted cost, `cost_usd`, to zero from now on. The task's `total_cost_usd` does not change: it is what the task has spent in total, not the budget counter.
The id names one plan or task; an id that names both is refused with exit `3` (see [Plan and task files](arc-files.md#plan-and-task-files)).

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

It applies only when the task's `rework_reason`, if there is one, comes from a verification stage (`commands`, `mutation`, `review`, or `ci`).
A reject note, a failed delivery check, a failed integration, and every other state are refused with exit `3`.
A task in `review.verifying` is accepted when its gate is stuck: its `runs/<task>/gate.json` record names the task's current attempt with status `no_record` or `died` (see [the gate run](arc-gate.md#the-gate-run)), and no gate process holds the task's gate lock.
Any other `review.verifying` task, including one whose gate is still running, is refused with exit `3`.
A plan paused by the automatic rework limit does not block `reverify` when the task's own reason is a failed verification stage.

A task whose `commands` or integration stage failed with `command_failed` or `timeout` is also re-verified by `radian arc run` once main moved (see [A command failure and a moved main](arc-gate.md#a-command-failure-and-a-moved-main)).
A task of a `cancelled` plan is refused with exit `3`.
That automatic path is the only one that re-runs an integration failure.
`reverify` ignores the automatic path's two guards: it runs whether or not the `(head_sha, main)` pair was re-verified before, and whether or not `limits.max_auto_reverify` is used up; it does not use up the count.

A reverify claims a new attempt without a worker:

- `attempt` goes up by one, and `attempt_id` and `token` are new;
- `head_sha` and `base_sha` stay the previous attempt's, and nothing is pushed;
- the attempt result's class is `reverify`;
- the task moves to `review.verifying` and is verified at once in the same command, as `start` does after a pushed attempt.

The new attempt's verdicts are its own: an earlier attempt's verdict is not reused and no verdict is ever rewritten.
The remote `work/<task>` must still equal `head_sha`; when it moved, `reverify` exits `3` with the moved-branch reason and the task stays in `review.verifying`, as verification does.

A reverify attempt is not a retry: it changes neither `retry_count` nor `max_attempts`, costs nothing, and is not an automatic rework (`auto_rework` does not change).

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
On `done.merged` it prints `<task> done.merged (<candidate sha>)`; a failed worktree removal is a `warning:` line after it and does not undo the merge, and so is a follow-up task that could not be added (see [The review's follow-up](arc-gate.md#the-reviews-follow-up)).

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
A task of a `cancelled` plan is never claimed, gated or merged, and a cancelled plan is not listed in the summary.

Before its first pass, and before each pass claims a task, it checks the worker's `radian exec` line the way `start` does: a rejected line exits `3`, claims nothing, and changes no file (see [`worker.exec_args`](arc-files.md#configtoml)).

| Flag | Value | Default | What it does |
|---|---|---|---|
| `--watch` | — | off | Repeat the loop: after a pass that ends quiet, wait `--interval` seconds and run another pass, until SIGINT or SIGTERM. Holds `runner.lock` the whole time. |
| `--interval` | `SECONDS` | `30` | Whole seconds between passes, 5 to 3600. Only with `--watch`: without it, or with a value outside 5 to 3600, the command is a usage error (`2`). |

One pass repeats until a pass changes nothing, in this order:

1. reconcile every task in `doing` (see [Reconcile](arc-gate.md#reconcile)): apply an `AttemptFinished` that is there, leave a live attempt alone, apply the retry rules to a dead one. A `doing.reconciling` task of a `cancelled` plan is ended at `failed.cancelled` after the same process check `cancel` makes;
2. move every `todo.retry_wait` task whose `retry_at` has passed back to `todo.ready`;
3. re-verify, by itself, a task a `commands` or integration command failure blocked, once main moved (see [A command failure and a moved main](arc-gate.md#a-command-failure-and-a-moved-main)): claim a new attempt without a worker. The two guards of that path apply, and an automatic reverify is not a retry: it does not count toward `max_attempts` or `auto_rework`;
4. for every task in `review.verifying` (every plan, including a task that a moved branch or a changed digest sent back, and one step 3 just claimed): apply the verdicts a background gate left, or start its gate in the background;
5. for each confirmed plan at `auto` `run` or `merge`, claim every `todo.ready` task whose `after` tasks are merged, up to `[limits] concurrency` attempts in flight, and start each wrapper without waiting for it;
6. for each plan at `auto` `merge`, approve a verified task and start its integration in the background, or finish an integration a gate left.
   `run` writes the same `human` pass verdict `radian arc merge` writes, with `approver` `auto` in it.

Steps 4, 5 and 6 walk the backlog in *claim order*: the plan's `priority` (`P0` first), then the plan id from small to large, then the task's order in the plan and its `after` list.
So the higher priority's task is verified, claimed, merged, and queued for a stage's lock before a lower priority's.
`priority` only decides which plan gets a free concurrency slot: a running attempt is never preempted, so a changed priority takes effect at the next free slot.

The long work runs outside the pass: a worker is a separate `radian arc work` process with its own limits and record, and a gate is a separate process with its own record.
A stage this build does not run, or a branch that moved, is reported in the summary; the gate's refusal is recorded, and `run` starts no new gate for that attempt until the inputs change (for verification the branch comes back; for integration a new `head_sha`, a new main or a changed `config.toml`; or `start`/`reverify` claims a new attempt).
A gate whose process exited without writing any record is reported as `<task> verify cannot run: its gate exited without a record` (or `<task> integration cannot run: …`), recorded for that attempt; no new gate starts while its inputs stay the same, and `show` prints the same text as the `rework_reason` (see [the gate run](arc-gate.md#the-gate-run)).
A gate that wrote `running` and then died is restarted, and the death is counted: after [`limits.max_gate_deaths`](arc-files.md#configtoml) deaths for the same attempt and inputs the record becomes `died`, the summary prints `<task> verify cannot run: its gate died after it started`, `show` prints the same text as the `rework_reason`, and no gate starts while the inputs stay the same. A gate the loop stopped itself is `interrupted` instead, and the next loop starts it again without counting.
The next pass applies a wrapper's early failure (a refused argument, a held worktree lock) the same way, so its concurrency slot is free at once (see [Reconcile](arc-gate.md#reconcile)).

A one-shot `run` starts its work and exits without waiting for it; the next `run` collects the result.
Use `--watch` to keep advancing.

`run` writes one line per transition on stdout, in order, and flushes it when the loop makes the transition; it does not wait for the command to end.
A started worker prints `<attempt id> started -> doing.running`; a started gate prints `<attempt id> verify started (pid <pid>)`, an approval prints `<task> approved -> review.integrating (<attempt id>)`, an integration prints `<task> integration started (pid <pid>)`, a conflict rework prints `<task> conflict with main -> todo.ready (automatic rework)`, or when the task has had `limits.max_conflict_rework` conflict reworks in a row, `<task> conflict with main -> review.rework_pending: <reason>`, an automatic reverify prints `<task> main moved (<short sha>) -> auto reverify`, or when the task used its count, `<task> main moved (<short sha>) -> auto reverify limit reached: <reason>`, and the follow-up task arc proposes when a task merges prints `<new task id> proposed (follow-ups from <task id>)` (see [The review's follow-up](arc-gate.md#the-reviews-follow-up)).
The limit's `<reason>` is the text `show` prints as the `rework_reason`: `the task used <n> automatic reverifies and `[limits] max_auto_reverify` is <max>`.
A task a plan cancel left at `doing.reconciling` prints `<task> doing.reconciling -> failed.cancelled (its plan is cancelled)`.
One line per `(head_sha, remote main)` pair is printed, so a summary that repeats every interval does not print it again.
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

A repeated error stops the loop: when three passes in a row return the same error text, it prints one line, `radian arc: the same error three passes in a row; stopping (fix it and start the loop again)`, stops the gates the way a stop signal does (see below; workers are left alone), releases `runner.lock`, and exits `1`.
A pass that succeeds, or a different error, starts the count again.
A one-shot `run` is unchanged.
The number three is a constant, not a configuration key.

On SIGINT or SIGTERM the loop starts no new worker and no new gate.
A Ctrl-C in the terminal reaches the loop only: each worker and each gate runs in its own process group, and both treat SIGINT exactly as SIGTERM.
A running worker is left alone: it is a separate process with its own limits and record, and the next pass (or the next `run`) collects it.
A running gate is signalled and gets `[limits] cancel_grace_secs` before SIGKILL, and so is one whose record is still `started` while it holds the gate lock; the gate forwards the signal to the stage it is running (the `commands` shell or the review session), waits for it, and exits without a record; once it is gone its record becomes `interrupted`, so a gate that did not finish is started again by the next loop as unverified and counts no death, and the verdicts it already wrote are reused, unless it exited before writing any record at all: then the next pass reports it and starts no gate while its inputs are unchanged (see [the gate run](arc-gate.md#the-gate-run)).
The loop then prints the summary, releases `runner.lock`, and exits `0`.

It exits `0` when it ran to quiet, even when a task failed, and `0` on SIGINT or SIGTERM with `--watch`; `1` on a git or file error, `3` when the worker's `radian exec` line is rejected, and `4` when another process holds `runner.lock`. It never waits for the lock.

## `radian arc mcp`

The lead's MCP server: registering it, its tools and its tool errors are in [`arc-mcp`](arc-mcp.md).

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

## The TUI

A bare `radian arc` opens the TUI; the plans page lists one of four views (`active` by default, then `all`, `done` and `cancelled`, cycled with `f`), and the plans, task and merge pages, every mark and key, and the owner's actions are in [`arc-tui`](arc-tui.md). The plans page's `x` cancels the selected task or plan, `c` confirms a draft plan or a proposed task (depending on the focused column), and `+`/`-` move the selected plan's priority; the page draws `P<n>` with the plan's id and in the frame label.

## Not built yet

The gate stages `mutation` and `ci` are not built: verification refuses a profile or task that lists one, and the task stays in `review.verifying`.

## See also

- `cli` for every other `radian` command.
- `environment` for the files radian reads, including this one.