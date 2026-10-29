---
description: The arc commands: every subcommand, the lead's MCP server, and the exit codes.
---

# arc

arc is a plan and task orchestrator built into the same `radian` binary.
It keeps a plan and its tasks as files in the repository, runs each task as a separate headless `radian` worker, and merges only what passes a fixed gate.

This build has the storage layer, the subcommands that read and write it: `init`, `add`, `list`, `show`, `adopt`, `confirm`, `auto`, `priority`, `pause`, `start`, `reverify`, `cancel`, `reset-budget`, `merge`, `reject`, `run`, and `serve`.
It also has `radian arc mcp`, the MCP server a lead writes plans and tasks through, the attempt runner behind `start`, and the gate and merge behind `start`, `merge` and the loop.
`radian arc serve` is the scheduling loop and `radian arc run` is one pass of it, and a bare `radian arc` opens the TUI, where you see the plans and tasks and act on them (see [`arc/tui`](tui.md)).

This page holds the `radian arc` commands; the scheduling loop is in [`arc/loop`](loop.md), the gate and its failures are in [`arc/gate`](gate.md), the lead's MCP server in [`arc/mcp`](mcp.md), the TUI in [`arc/tui`](tui.md), the flows that give tasks their worker and gate in [`arc/flows`](flows.md), the worker in [`arc/worker`](worker.md), and the files arc keeps are in [`arc/files`](files.md).

## `radian arc`

`radian arc` with no subcommand opens the TUI; its pages and keys are in [`arc/tui`](tui.md).
Without setup, `radian arc` and every subcommand except `init` refuse with a line that names `radian arc init` and exit `3`; without a terminal, `radian arc` exits `2`.

## `radian arc init [--track]`

`init` sets arc up for this repository.
It runs anywhere inside the working tree and works on the repository root.

It finds:

- the repository root (`git rev-parse --show-toplevel`);
- the remote: the `[repository] remote` of a `config.toml` that already exists, else `origin` when the repository has it, else the repository's only remote;
- the main branch: that remote's own HEAD when it is known, else the checked-out branch, else `main`.

Several remotes and no `origin` are ambiguous, so `init` refuses and names each one.

It writes `config.toml` with those values, and it creates the layout below.
It writes `worker-brief.md` with a short default when it does not exist, and never overwrites an existing file (see [the worker brief](files.md#worker-briefmd)).
A second `init` keeps an existing `config.toml` byte for byte and creates nothing twice.

It adds the arc paths to `.git/info/exclude`, a local file: `init` never changes a committed `.gitignore`.
Each line is added once; a second run adds nothing.
`init` writes no other file in the repository's git dir: the first fetch arc runs creates `<git common dir>/radian-arc-fetch.lock`, the lock that makes its own fetches into the repository take turns, and every worktree of the repository shares it (see [the gate run](gate.md#the-gate-run)).

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
| `/.radian/arc/worker-brief.md` | excluded | excluded |

`--track` makes git see `config.toml`, `plans/`, and `tasks/`, so you can commit them when you want; arc never commits them.
Removing a line that an earlier run added is up to you: `init` only appends.

Then it checks its requirements and prints one line per check:

| Check | What it runs |
|---|---|
| `provider credential` | Reads the stored credential (`radian login` writes it to `~/.radian/credentials/auth.json`). No entry is a warning. |
| `` `<remote>` reachable `` | `git ls-remote <remote>`. |
| `` push to `<remote>` `<main>` `` | `git push --dry-run <main>:<main>`. A refusal here means the first real push will be refused too. |
| `workspace sandbox` | Builds a `workspace` sandbox over the repository: `sandbox-exec` on macOS, bubblewrap on Linux. |

A failed check is a warning line, not a refusal: `init` still exits `0`, because a missing credential or a slow remote is something you can fix after setup.

Finally it prints the command that registers the arc MCP server with a lead.
It does not change any configuration of its own:

```sh
claude mcp add arc -- radian arc mcp
```

`init` exits `0` when it wrote the layout and `1` when the directory is not a git working tree, the repository has no remote, or it has several remotes and none is `origin`.

## `radian arc add`

`add` creates one plan or one task from standard input.
The new id is printed on stdout, one line.
Plans and tasks count in separate number sequences, so a plan and a task may carry the same number; the prefix tells them apart (see [Plan and task files](files.md#plan-and-task-files)).

### `radian arc add plan --slug <SLUG> --title <TITLE>`

Reads the request text from stdin to end of input, writes `plans/<id>.md`, and prints the id.

| Flag | Value | What it does |
|---|---|---|
| `--slug` | `SLUG` | The part of the id after the prefix and number: 1 to 48 characters from `a-z`, `0-9`, `-`, with no leading or trailing `-`. Required. |
| `--title` | `TITLE` | The plan's title. Required. It may not be empty. |

The body is the request text, then a `## Change records` section.
A confirmed plan may only gain change records there; the request text itself is never rewritten.

### `radian arc add task --plan <ID> --slug <SLUG> --title <TITLE> [--after <ID>]... [--allow-noop [BOOL]] [--stage <NAME>]... [--flow <NAME>]`

Reads the brief from stdin to end of input, writes `tasks/<id>.md`, appends the id to the plan's `tasks` list, and prints the id.

| Flag | Value | What it does |
|---|---|---|
| `--plan` | `ID` | The plan the task belongs to. Required. |
| `--slug` | `SLUG` | The same slug rule as a plan. Required. |
| `--title` | `TITLE` | The task's title. Required. It may not be empty. |
| `--after` | `ID` | A task that must be merged first. Repeat the flag for several. |
| `--allow-noop` | `BOOL` | The task's own `allow_noop`: whether it may deliver no change on purpose. The flag alone means `true`; `--allow-noop false` sets an explicit `false`. Without the flag the field is not written, so the task uses its flow's [`flows.<name>.allow_noop`](files.md#configtoml). |
| `--stage` | `NAME` | An extra gate stage to run for this task. Repeat the flag. The value is `commands`, `mutation`, `review`, `ci`, or `human`. |
| `--flow` | `NAME` | The flow whose worker every attempt starts: a `[flows.<name>]` section of `config.toml` ([the flow rows](files.md#flows)). Default: `default`. A name the file does not define exits `3` and names the flows it does. |

A task can only be added to a plan that is `draft` and not cancelled by its tasks; a confirmed or cancelled plan exits `3`.
An `--after` id must name a task of the same plan or a task whose state is `done`; anything else exits `3`.

## `radian arc list [--plan <ID>] [--all] [--json]`

Lists plans in claim order — the order `radian arc run` serves them in: the higher `priority` first (`P0` before `P1`), then the plan id — each with its tasks in plan order.
Each plan's header line shows its id, `status`, `auto` level, `priority`, progress as `<merged>/<tasks> merged`, `total_cost_usd` (its tasks' totals sum) and title.
The progress total leaves out `failed.cancelled` tasks: one merged task and one cancelled follow-up read `1/1`, an all-cancelled plan `0/0`.
Each task line shows `state.phase`, the attempt number, and `retry at <UTC time>` when the task has a `retry_at`.
When the repository defines a flow other than `default`, each task line also shows the task's flow in its own column, right after the id; a task without a `flow` field prints `default`.
A repository whose `config.toml` has no `[flows]` table prints no flow column.
A `todo.ready` task that the next pass would not claim because a limit is full ends its line with `(waiting for a free <flow> slot)`, or with `(waiting for a free concurrency slot)` when the repository total is full and the task's flow has room; when both are full the flow's reason wins (see [the queue reason](#trying-it-first-dry-run)).
A task held by an `after` task that has not merged, by a plan whose `auto` is `off`, by a flow `config.toml` does not define, or by its own file or its plan having changed outside arc gets no such reason.
A task's `cost_usd` is what the budget counts (every worker attempt and every `review` stage) and what `reset-budget` sets back to `0`; its `total_cost_usd` is the same spend and is never reset.
An attempt whose worker reported no cost (a [`worker.command`](files.md#configtoml) that writes no `cost` in its `Result`, or a flow's [`acp`](files.md#configtoml) agent, whose protocol has no fee) adds nothing to either number.
arc records that on the task as [`cost_unknown`](files.md#task-fields), and a task that carries it — or a plan with such a task — prints `unknown` instead of a number, even after a later attempt reports a cost.

By default finished work is left out, so you see the work in progress:

- every `done.merged` task is left out; a shown plan still counts them in its header;
- a **finished** plan is left out: at least one task, every task `done.merged` or `failed.cancelled`, at least one `done.merged`. A plan with no tasks is shown;
- a `cancelled` plan is left out and `--all` shows it: its `status` is `cancelled`, or every task is `failed.cancelled`; that derives from its tasks and never rewrites `status`.

Every other state is shown, `failed.*` included; a `failed.cancelled` task stays listed (and in `show`).

When the default left something out, a last line counts it:

```text
3 finished plans and 12 merged tasks hidden (--all to show)
```

The count covers every plan and merged task not printed. A `0` part is omitted and `1` is singular: `1 finished plan and 1 merged task hidden (--all to show)`, `2 cancelled plans and 3 merged tasks hidden (--all to show)`, or `2 merged tasks hidden (--all to show)` when no whole plan was left out.
When everything is hidden, that line is the whole output; nothing hidden means no line.

| Flag | Value | What it does |
|---|---|---|
| `--plan` | `ID` | Only this plan, following the default filter; a finished or cancelled plan still prints its header line, so you see that it exists. |
| `--all` | — | No filter: print every plan and every task, `done.merged` history and cancelled plans included. It takes no value. |
| `--json` | — | Print one JSON array instead of lines. Without `--all` it follows the same cancelled-plan filter (by `status` or by its tasks); a plan named with `--plan` is always included. |

A plan or task whose file does not match arc's record is marked `(changed outside arc)`.

The JSON array holds one object per plan: `id`, `title`, `status`, `auto`, `priority`, `max_cost_usd`, `max_wall_time_secs`, `cost_usd` (its tasks' `cost_usd` sum, the number the plan budget compares), `total_cost_usd` (its tasks' totals sum), `cost_unknown` (`true` when one of its tasks is), `changed_outside_arc`, and `tasks`.
Each task object holds `id`, `plan`, `flow`, `title`, `state`, `phase`, `queue_reason`, `attempt`, `attempt_id`, `token`, `cost_usd`, `total_cost_usd`, `cost_unknown`, `auto_rework`, `retry_count`, `after`, `supersedes`, `allow_noop`, `allow_noop_source`, `followup_pending`, `pending_hooks`, `merged_sha`, `extra_stages`, `max_attempts`, `max_cost_usd`, `max_wall_time_secs`, `stall_timeout_secs`, `base_sha`, `head_sha`, `verified_sha`, `verdict_set_digest`, `retry_at`, and `changed_outside_arc`.
`allow_noop` is the effective value: the task's own when its file carries the field, else its flow's. `allow_noop_source` is `task` or `flow`, the one it came from.
`flow` is the task's flow name, and `default` when its file has no `flow` field.
`queue_reason` is the slot reason the task line prints (`waiting for a free <flow> slot` or `waiting for a free concurrency slot`), or `null` when no limit holds the task.

## `radian arc show <ID> [--json]`

Prints one plan or task: every field, whether it changed outside arc, then its body.
For a task it also prints the current attempt's notes as `attempt_note`, such as `worker-brief.md skipped: not UTF-8` (see [the worker brief](files.md#worker-briefmd)).
An attempt that ended without reaching the gate (an infrastructure or model failure) prints its reason as `attempt_failure`: a `before_attempt` command's exit code or timeout and log path, a failed fetch, another attempt's lock, and so on. A task that has a `rework_reason` prints that reason instead, so the two never repeat it.
A task prints `queue_reason` after `phase`: the slot the next pass would not claim it for, in the words `list` prints, or `-` when no limit holds it (a task or plan that changed outside arc included).
A task prints `gate_profile` after `flow`: the profile the newest verdict that records one was made under, or `-` until such a verdict exists.
`show --json` carries the notes as `attempt_notes`, a list of strings (empty when there are none), and `attempt_failure` as a string or `null`.

A plan prints `cost_usd` and `total_cost_usd` beside its other fields: the first is the sum of its tasks' `cost_usd` and the number its budget compares, the second is the sum of their totals. Both are computed when the plan is read and are not stored in the plan file. A task prints both of its own numbers.
A task whose attempt reported no cost prints `unknown` for both, and a plan with such a task prints `unknown` for both of its sums; `show --json` keeps the numbers it has and sets `cost_unknown` to `true`, so a script can tell an incomplete sum from a complete one. The state stays after a later attempt reports a cost; see [Another agent as the worker](worker.md#another-agent-as-the-worker).

The id names one plan or task. An id that names both a plan and a task is refused with exit `3`, naming both files (see [Plan and task files](files.md#plan-and-task-files)).
`list --plan <id>` always reads the plan.

| Argument | Value | What it does |
|---|---|---|
| `<ID>` | a plan or task id | Required. An id that names both a plan and a task is refused (`3`). |

| Flag | Value | What it does |
|---|---|---|
| `--json` | — | Print one JSON object instead of fields. |

The object holds the same keys as in `list --json` (`queue_reason` included), plus `body`, `session_id`, `gate_profile`, `rework_reason`, `attempt_failure`, `attempt_model_error`, the `after` chain's `waits_on`, `blocks`, `never_runs` and `cycle` (see [the `after` chain](#the-after-chain)), and the newest review verdict's `review`, `review_verdict`, `review_session_ids`, `review_cost_micros`, `review_model_error` and `review_settings`.
`allow_noop` is the effective value, and `allow_noop_source` is `task` or `flow`, the one it came from.
An unset field is `null`; `session_id` is the current attempt's worker session id, from its `execution.json`, else its `identity.json`, else the claim's `start.json`, and `null` only when none of them names one; `rework_reason` is `null` unless the task is in `review.verifying` or `review.rework_pending` and has a recorded reason (a failed stage, a moved branch, a gate that left no record, a gate that died after it started, a reject note, the automatic rework limit that paused the plan, a conflict that reached `limits.max_conflict_rework`, an automatic reverify count that is used up, or a review waiting on an unavailable model, whose text is `review_unavailable: model <kind> (HTTP <status>), retry at <UTC time>`). `retry_at` is the absolute UTC time a waiting retry, a worker model wait or a review wait ends. `attempt_failure` is the current attempt's own failure reason when it ended before the gate, and `null` when it has none or when `rework_reason` carries the reason. `attempt_model_error` is the `model_error` object of a worker attempt that waited on its model, or `null` (see [A worker wait on an unavailable model](gate.md#a-worker-wait-on-an-unavailable-model)).
`review` is `{"summary", "blocking", "suggestions", "verified"}` from the newest `review` verdict, or `null`; each item of `blocking` and `suggestions` is an object with `text` and, when it has them, `level`, `file`, `line` and `pre_existing` (see [the review stage](gate.md#the-gate-run)); `review_verdict` is its path or `null`; `review_session_ids` lists every session the stage ran; `review_cost_micros` is their summed cost in micros of a US dollar (`null` when the run reported none); `review_model_error` is the failing session's `Result.model_error` object, or `null` (see [A review wait on an unavailable model](gate.md#a-review-wait-on-an-unavailable-model)); `review_settings` is `{"round", "exec_args", "timeout_secs", "timeout"}` (the round `first` or `rework`, the config key the session's arguments came from, its wall-time limit in seconds and the key that limit came from), or `null`.
For a task with a review verdict, the text form also prints `review_verdict`, `review_summary`, `review_blocking`, `review_suggestions`, `review_verified`, `review_session_ids`, `review_cost_micros`, and `review_round`, `review_exec_args`, `review_timeout_secs` and `review_timeout`.
The text form prints `session_id` after `attempt_id`.
An id that names nothing exits `1`.

### The `after` chain

A task's `after` lists the tasks that must be merged before it is claimed (see [Plan and task files](files.md#plan-and-task-files)).
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

It refuses with exit `3` when the id names both a plan and a task (see [Plan and task files](files.md#plan-and-task-files)), or when the outside change touched a field only arc (for a task) or the owner (for a plan) sets:

- a task: `state`, `phase`, `attempt`, `attempt_id`, `token`, `cost_usd`, `total_cost_usd`, `cost_unknown`, `auto_rework`, `retry_count`, `followup_pending`, `pending_hooks`, `merged_sha`, `retry_at`, `base_sha`, `head_sha`, `verified_sha`, `verdict_set_digest`;
- a plan: `status`, `auto`, `priority`.

A file that already matches the ledger is a no-op and prints so.
Without a recorded version to compare against, `adopt` refuses: it cannot tell which fields changed.

## `radian arc confirm <ID>`

`confirm` is your action: it accepts a draft plan, or a task arc or the lead proposed.

A plan moves from `draft` to `confirmed`.
A confirmed plan gets no new `todo.ready` tasks, and its request text can only gain change records (see [Plan and task files](files.md#plan-and-task-files)).
A plan's `auto` level may only be other than `off` once it is confirmed.

A task moves from `todo.proposed` to `todo.ready`.
The plan's `status` and `auto` do not change, and the plan does not return to `draft`.
A task waits in `todo.proposed` until you confirm it when arc proposed it as the plan's review follow-up (see [The review's follow-up](gate.md#the-reviews-follow-up)) or when the lead added it to a confirmed plan with the MCP `add_task` tool; the scheduling loop passes over it, and `start` refuses it.
A proposed task you do not want is ended with `radian arc cancel` (`failed.cancelled`).

| Argument | Value | What it does |
|---|---|---|
| `<ID>` | a plan id, or a task id | Required. An id that names both a plan and a task is refused (`3`), naming both files. |

A plan confirm refuses with exit `3` when the plan has no tasks, when it is already confirmed, when it is cancelled, when its file changed outside arc, and when the worker's `radian acp` line is rejected (see `start` and [`worker.exec_args`](files.md#configtoml)).
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
- one `<id> queue: <n> of <m> todo.ready` line per such task: its place among every `todo.ready` task plus itself, in claim order (the plan's `priority`, the plan id, the task's order in the plan). A `todo.ready` task no plan lists, possible only through an adopted plan edit, sorts last.

When the next pass would not claim the task, the queue line ends with the reason in parentheses, in the words `radian arc run`'s summary uses: `(waiting for <id> to merge)`, `(waiting for a free <flow> slot)`, `(waiting for a free concurrency slot)`, or ``(waiting for you to start it (`auto` is `off`))``.
The flow reason appears when the task's flow has its [`concurrency`](files.md#configtoml) tasks in `doing` (for example `(waiting for a free image slot)`); the shared reason appears when [`limits.concurrency`](files.md#configtoml) is full, and the flow's reason wins when both are full.
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
`priority` only decides which plan gets the next free concurrency slot, the task's flow's or the repository's; it never preempts an attempt that is already running, so a change takes effect at the next pass that has a free slot (see [`run`](#radian-arc-run)).
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

Before it reconciles, claims or verifies anything, `start` checks the worker line (see the [`worker.exec_args` and `worker.command` rows](files.md#configtoml)).
Without `[worker] command`, it parses the line the worker would get: the wrapper's fixed `radian acp` arguments plus `[worker] exec_args`.
A line `radian acp` would reject, or one that repeats an argument the wrapper sets, refuses the command with exit `3` and the parser's own message, names `worker.exec_args`, claims nothing, and changes no file.
With `[worker] command` set, arc checks instead that the program resolves (see [Another agent as the worker](worker.md#another-agent-as-the-worker)); a program that does not resolve refuses the command the same way, naming `worker.command` and the program. A flow's `acp` or `command` program is checked the same way and named by its own key.

Before it claims anything it reconciles every task in `doing` (see [Reconcile](gate.md#reconcile)) and moves a `todo.retry_wait` task whose `retry_at` has passed back to `todo.ready`.

`start` also claims the task you name when it is `todo.retry_wait`, even while its `retry_at` is still in the future: your `start` is a run-now, and the backoff exists for the scheduling loop. The claim is the same transaction as a `todo.ready` one, with the same concurrency check and the same printed line, and it clears `retry_at`.

A claim is refused with exit `3` and changes nothing when:

- the task is not `todo.ready` or `todo.retry_wait`; a `todo.proposed` task names its phase and waits for `radian arc confirm`, and `todo.paused_budget` and every `failed.*` phase are refused;
- its plan is not `confirmed`; a `cancelled` plan is refused, naming its status;
- an `after` task is not `done`;
- its flow already has [`flows.<flow>.concurrency`](files.md#configtoml) tasks in `doing`;
- [`limits.concurrency`](files.md#configtoml) tasks are already in `doing`.

The refusal names the full limit and its count: `flows.<name>.concurrency` or `limits.concurrency`. When both are full, it names the flow's limit.

The claim is one transaction: the task's `attempt` goes up by one, `attempt_id` becomes `<task>-<attempt>`, a random `token` is written, and the task moves to `doing.starting`.
arc then runs the hidden `radian arc work` wrapper as a child, with the brief on its stdin; the wrapper moves the task to `doing.running`.
The worker itself — the session it runs in, the brief it receives, the steps before an attempt, the limits the wrapper enforces, and the checks after it exits — is in [the worker](worker.md).
When [`worker.command`](files.md#configtoml) is set, another agent runs in its place: see [Another agent as the worker](worker.md#another-agent-as-the-worker). A flow can also start an ACP agent with [`flows.<name>.acp`](files.md#configtoml); see [An ACP agent as the worker](worker.md#an-acp-agent-as-the-worker).

When the attempt ends, arc applies the result in one transaction (the [result table](gate.md#failures-retries-and-the-budget)) and prints one line:

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

For example `TK0001-work-1 verify -> review.awaiting_human`, or `TK0001-work-1 verify -> review.rework_pending (commands: command_failed (verdict /…/verdicts/commands.json))`. A review whose model is unavailable prints `<task> review model unavailable -> retry at <UTC time>` instead (see [A review wait on an unavailable model](gate.md#a-review-wait-on-an-unavailable-model)).
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
- the integration stage failed with reason `command_failed` or `timeout`, the same stage and reason pair the automatic reverify of a moved target branch accepts (see [A command failure and a moved target branch](gate.md#a-command-failure-and-a-moved-target-branch)); the target branch does not need to have moved.

A reject note, a failed delivery check, an integration failure with any other reason (a `conflict` has its own rework path, in [A conflict with the target branch](gate.md#a-conflict-with-the-target-branch)), and every other state are refused with exit `3`.
A task in `review.verifying` is accepted when its gate is stuck: its `runs/<task>/gate.json` record names the task's current attempt with status `no_record` or `died` (see [the gate run](gate.md#the-gate-run)), and no gate process holds the task's gate lock.
Any other `review.verifying` task, including one whose gate is still running, is refused with exit `3`.
A plan paused by the automatic rework limit does not block `reverify` when the task's own reason is one of the failures above.

A task whose `commands` or integration stage failed with `command_failed` or `timeout` is also re-verified by `radian arc run` once the target branch moved (see [A command failure and a moved target branch](gate.md#a-command-failure-and-a-moved-target-branch)). A task waiting on an unavailable review model is re-verified by `radian arc run` once its `retry_at` passes (see [A review wait on an unavailable model](gate.md#a-review-wait-on-an-unavailable-model)).
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
It runs no worker, so no worker cost is added, but the `review` stage it runs counts: its cost is added to the task's `cost_usd` and `total_cost_usd` when the gate's verdicts are applied, and it can pause the task in `todo.paused_budget` (see [`limits.max_cost_usd`](files.md#configtoml)).

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

## `radian arc merge <TASK>`

`merge` takes your approval, builds the integration candidate, pushes it to the remote target branch, and finishes the task.
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
2. runs the integration stage against the current remote target branch (`[integration] target_branch`, else `[repository] main_branch`). The candidate is `head_sha` when the target is still the task's `base_sha`; otherwise it is a merge commit of `head_sha` onto the target (arc never rebases). The current `[integration] key_commands` run on the candidate.
   A conflict or a failing key command moves the task to `review.rework_pending` with the reason, and `merge` exits `0`.
   A passing integration moves the task to `review.ready_to_merge`.
3. pushes the candidate to the remote target branch without force: `git push <remote> <candidate>:refs/heads/<target>`.
   When the target moved after integration (or the push is rejected as non-fast-forward), the task returns to `review.integrating` and `merge` integrates again, at most 3 times in one command. After that it exits `3` with the task in `review.integrating`; a later `merge` continues from there.
4. checks with `git ls-remote` that the remote target branch is the candidate and that the candidate contains `head_sha`, and moves the task to `done.merged`, recording the `after_merge` hook and `merged_sha` in the same write. arc itself removes nothing; the default `[hooks] after_merge` lines remove the task's worktree (`.radian/arc/worktrees/<task>`) and its branches (see [Hooks](#hooks)).
   The task's branch stays on the remote until a hook deletes it.

`merge` never checks out or updates the main checkout's working tree: it fetches, pushes, and reads refs only.
On `done.merged` it prints `<task> done.merged (<candidate sha>)`, followed by `<new task id> proposed (follow-ups from <plan id>)` when the merge left the plan with no open task and it has review items to collect (see [The review's follow-up](gate.md#the-reviews-follow-up)). A hook command that fails is a `warning:` line after it and does not undo the merge, and so is a follow-up task that could not be added. When a scheduling loop holds `runner.lock`, the command leaves its pending hook for the loop; otherwise it runs it itself before it prints.

The exit codes are the command's own:

### Merge exit codes

| Code | Meaning | When |
|---|---|---|
| `0` | success | The task reached `done.merged`, or the integration sent it to `review.rework_pending`. |
| `1` | an error | A git or file error. |
| `3` | the state does not allow the operation | The state does not allow the merge, the branch moved, the digest changed (the task returns to `review.verifying`), or 3 integration runs did not settle (the task stays in `review.integrating`). |
| `4` | a lock is held by another process | Another process holds `state.lock`. |

## Hooks

`[hooks]` runs configured shell commands at fixed points in a task's life.
The point names and their variables are fixed by the binary; only the command lines come from `config.toml`.
Each point is an array of shell command lines, default `[]`:

| Point | Fires after |
|---|---|
| `after_merge` | A task enters `done.merged`. |
| `after_fail` | A task enters any `failed.*` phase; a plan cancel records one point per task it ends. |
| `after_rework` | A task enters `review.rework_pending`; a conflict rework back to `todo.ready` is not this point. |
| `after_awaiting_human` | A task enters `review.awaiting_human`. |
| `after_proposed` | A new task enters `todo.proposed` (MCP `add_task` or the review follow-up); the MCP process only records it. |

[`radian arc init`](#radian-arc-init-track) writes three `after_merge` lines that delete the task's remote branch, worktree and local branch, naming the configured remote; a repository that wants to keep one removes its line. The commands read `[hooks]` when they run, not when the task moved. See [the config keys](files.md#configtoml) for the default lines.

**Rules.** The write that moves a task into one of those phases records the point in [`pending_hooks`](files.md#task-fields) in the same transaction; a write that leaves the phase as it is records nothing. Only the holder of `runner.lock` runs hooks: every `run`/`serve` pass runs them first, and the owner's `merge`, `reject`, `cancel`, `start` and `reverify` run them after their transition when they can take the lock without waiting; the MCP server never runs one. A point is cleared only after its commands ran, so a killed pass runs it again: **a hook command must be safe to run twice**. A failing or timed-out command logs one `warning:` line and the next still runs; hooks never change a task's state or make a command fail. Each command's timeout is [`gate.command_timeout_secs`](files.md#configtoml), and its output goes to `runs/<task>/hooks/<point>-NN.log` (see [the layout](files.md#the-radianarc-layout)).

**Environment.** Every command receives these variables on top of arc's own environment:

| Variable | Value |
|---|---|
| `ARC_TASK` | The task id. |
| `ARC_PLAN` | The task's plan id. |
| `ARC_ATTEMPT` | The task's current attempt, `<task>-<n>`. |
| `ARC_BRANCH` | The task's branch, `work/<task>`. |
| `ARC_WORKTREE` | The task's worktree, absolute: `.radian/arc/worktrees/<task>`. |
| `ARC_TARGET_BRANCH` | The branch the merge targeted: [`integration.target_branch`](files.md#configtoml), else [`repository.main_branch`](files.md#configtoml). |
| `ARC_MERGED_SHA` | The sha the merge pushed to the target branch. `after_merge` only. |
| `ARC_STATE` | The task's state and phase as `<state>.<phase>` when the command runs. |
| `ARC_HOOK` | The point name. |

**A phone notification.** This `after_awaiting_human` command sends only the repository name, task id and phase; it carries no transcript, command text, model, usage or cost:

```toml
[hooks]
after_awaiting_human = [
  "curl -s -X POST https://example.com/arc -H 'Content-Type: application/json' -d \"{\\\"repo\\\":\\\"\"$(basename \"$PWD\")\"\\\",\\\"task\\\":\\\"$ARC_TASK\\\",\\\"phase\\\":\\\"$ARC_STATE\\\"}\"",
]
```

The hook runs in the repository root; `$(basename "$PWD")` is its directory name. `after_fail` and `after_proposed` are the other points a notification usually watches.

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

`run` is one pass of the scheduling loop: what a pass does, its output, its exit codes and `loop.log` are in [`arc/loop`](loop.md#radian-arc-run).

## `radian arc serve`

`serve` is the scheduling loop: `--interval`, its output, the signal handling and the repeated-error stop are in [`arc/loop`](loop.md#radian-arc-serve).

## `radian arc mcp [--claude-experimental]`

`radian arc mcp` is the stdio MCP server the lead writes plans and tasks through. Registering it, its tools and its tool errors are in [`arc/mcp`](mcp.md).

| Flag | Value | Default | What it does |
|---|---|---|---|
| `--claude-experimental` | — | off | Push task transitions to a Claude Code channel. The details are in [`arc/mcp`](mcp.md#the-claude-experimental-flag). |

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

The TUI never holds `runner.lock` and runs no pass. When it opens, it starts `radian arc serve` with the default interval as a detached process if no loop holds the lock; `R` starts one the same way while none runs.
The header shows `loop ● running (pid <n>)` for any holder (the pid appears when the holder recorded a readable one, otherwise it is plain `loop ● running`), `loop ◌ starting` until the loop it started records its own pid in `runner.lock`, and `loop ○ stopped` when none runs; a failed start adds one footer line with its reason and exit code.
The newest transition line `loop.log` gained since the TUI opened is the footer's ticker, and `L` opens a view of the last 500 lines.
`q` and SIGTERM close the TUI and nothing else: a loop it started keeps running, and an attempt it left running is reconciled by the next loop.
A running task's diff tab shows the worker's worktree against the attempt's `base_sha`, uncommitted files included (`live · worktree`), refreshed every 5 seconds.
A bare `radian arc` opens the TUI; the plans, board, task and merge pages, every mark and key, and the owner's actions are in [`arc/tui`](tui.md).
The plans page shows one of four views (`active` by default, then `all`, `done`, `cancelled`; `f` cycles them) and each plan draws one of seven marks — `✕` cancelled, `◌` draft, `◉` needs you, `●` running, `◍` paused, `○` queued, `✓` done — in its own `plan_*` theme role, with `P<n>` by its id and in the frame label.
Its `x`, `c`, `+`/`-` and `o` cancel, confirm, step the priority and cycle the order (`priority`, `state`, `id`); the mark rules and the columns are in [the plans page](tui.md#the-plans-page).

## Not built yet

The gate stages `mutation` and `ci` are not built: verification refuses a profile or task that lists one, and the task stays in `review.verifying`.

## See also

- `cli` for every other `radian` command.
- `environment` for the files radian reads, including this one.
- `arc/worker` for the worker itself: its session, brief and limits, and another agent as the worker.