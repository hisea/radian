---
description: The commands that write plans and tasks: init, add, list, show and adopt.
---

# arc: plans and tasks

The plan and task commands.

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
| `--allow-noop` | `BOOL` | The task's own `allow_noop`: whether it may deliver no change on purpose. The flag alone means `true`; `--allow-noop false` sets an explicit `false`. Without the flag the field is not written, so the task uses its flow's [`flows.<name>.allow_noop`](config.md#configtoml). |
| `--stage` | `NAME` | An extra gate stage to run for this task. Repeat the flag. The value is `commands`, `mutation`, `review`, `ci`, or `human`. |
| `--flow` | `NAME` | The flow whose worker every attempt starts: a `[flows.<name>]` section of `config.toml` ([the flow rows](config.md#flows)). Default: `default`. A name the file does not define exits `3` and names the flows it does. |

A task can only be added to a plan that is `draft` and not cancelled by its tasks; a confirmed or cancelled plan exits `3`.
An `--after` id must name a task of the same plan or a task whose state is `done`; anything else exits `3`.

## `radian arc list [--plan <ID>] [--all] [--json]`

Lists plans in claim order — the order `radian arc run` serves them in: the higher `priority` first (`P0` before `P1`), then the plan id — each with its tasks in plan order.
Each plan's header line shows its id, `status`, `auto` level, `priority`, progress as `<merged>/<tasks> merged`, `total_cost_usd` (its tasks' totals sum) and title.
The progress total leaves out `failed.cancelled` tasks: one merged task and one cancelled follow-up read `1/1`, an all-cancelled plan `0/0`.
Each task line shows `state.phase`, the attempt number, and `retry at <UTC time>` when the task has a `retry_at`.
When the repository defines a flow other than `default`, each task line also shows the task's flow in its own column, right after the id; a task without a `flow` field prints `default`.
A repository whose `config.toml` has no `[flows]` table prints no flow column.
A `todo.ready` task that the next pass would not claim because a limit is full ends its line with `(waiting for a free <flow> slot)`, or with `(waiting for a free concurrency slot)` when the repository total is full and the task's flow has room; when both are full the flow's reason wins (see [the queue reason](owner.md#trying-it-first-dry-run)).
A task held by an `after` task that has not merged, by a plan whose `auto` is `off`, by a flow `config.toml` does not define, or by its own file or its plan having changed outside arc gets no such reason.
A task's `cost_usd` is what the budget counts (every worker attempt and every `review` stage) and what `reset-budget` sets back to `0`; its `total_cost_usd` is the same spend and is never reset.
An attempt whose worker reported no cost (a [`worker.command`](config.md#configtoml) that writes no `cost` in its `Result`, or a flow's [`acp`](config.md#configtoml) agent, whose protocol has no fee) adds nothing to either number.
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
Each task object holds `id`, `plan`, `flow`, `title`, `state`, `phase`, `queue_reason`, `attempt`, `attempt_id`, `token`, `cost_usd`, `total_cost_usd`, `cost_unknown`, `auto_rework`, `retry_count`, `after`, `supersedes`, `allow_noop`, `allow_noop_source`, `followup_pending`, `pending_hooks`, `pending_hook_states`, `merged_sha`, `extra_stages`, `max_attempts`, `max_cost_usd`, `max_wall_time_secs`, `stall_timeout_secs`, `base_sha`, `head_sha`, `verified_sha`, `verdict_set_digest`, `retry_at`, and `changed_outside_arc`.
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
An unset field is `null`; `session_id` is the current attempt's worker session id, from its `execution.json`, else its `identity.json`, else the claim's `start.json`, and `null` only when none of them names one; `rework_reason` is `null` unless the task is in `review.verifying` or `review.rework_pending` and has a recorded reason (a failed stage, a moved branch, a gate that left no record, a gate that died after it started, a reject note, the automatic rework limit that paused the plan, a conflict that reached `limits.max_conflict_rework`, an automatic reverify count that is used up, or a review waiting on an unavailable model, whose text is `review_unavailable: model <kind> (HTTP <status>), retry at <UTC time>`). `retry_at` is the absolute UTC time a waiting retry, a worker model wait or a review wait ends. `attempt_failure` is the current attempt's own failure reason when it ended before the gate, and `null` when it has none or when `rework_reason` carries the reason. `attempt_model_error` is the `model_error` object of a worker attempt that waited on its model, or `null` (see [A worker wait on an unavailable model](gate-failures.md#a-worker-wait-on-an-unavailable-model)).
`review` is `{"summary", "blocking", "suggestions", "verified"}` from the newest `review` verdict, or `null`; each item of `blocking` and `suggestions` is an object with `text` and, when it has them, `level`, `file`, `line` and `pre_existing` (see [the review stage](gate-review.md#the-review-stage)); `review_verdict` is its path or `null`; `review_session_ids` lists every session the stage ran; `review_cost_micros` is their summed cost in micros of a US dollar (`null` when the run reported none); `review_model_error` is the failing session's `Result.model_error` object, or `null` (see [A review wait on an unavailable model](gate-review.md#a-review-wait-on-an-unavailable-model)); `review_settings` is `{"round", "exec_args", "timeout_secs", "timeout"}` (the round `first` or `rework`, the config key the session's arguments came from, its wall-time limit in seconds and the key that limit came from), or `null`.
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

- a task: `state`, `phase`, `attempt`, `attempt_id`, `token`, `cost_usd`, `total_cost_usd`, `cost_unknown`, `auto_rework`, `retry_count`, `followup_pending`, `pending_hooks`, `pending_hook_states`, `merged_sha`, `retry_at`, `base_sha`, `head_sha`, `verified_sha`, `verdict_set_digest`;
- a plan: `status`, `auto`, `priority`.

A file that already matches the ledger is a no-op and prints so.
Without a recorded version to compare against, `adopt` refuses: it cannot tell which fields changed.

