---
description: Plan and task orchestration: setup, plans, tasks, the files arc keeps, the lead's MCP server, and its exit codes.
---

# arc

arc is a plan and task orchestrator built into the same `radian` binary.
It keeps a plan and its tasks as files in the repository, runs each task as a separate headless `radian` worker, and merges only what passes a fixed gate.

This build has the storage layer and the subcommands that read and write it: `init`, `add`, `list`, `show`, `adopt`, `confirm`, `auto`, `pause`, `start`, `cancel`, and `reset-budget`.
It also has `radian arc mcp`, the MCP server a lead writes plans and tasks through, and the attempt runner behind `start` and `cancel`.
The gate library and the worker wrapper are internal parts of the binary; no subcommand runs the gate yet.
The scheduling loop, merge, and the arc terminal interface are **not built yet**, so the `run` and `merge` auto levels are stored and nothing acts on them in the background.

## `radian arc`

`radian arc` with no subcommand would open the arc interface.
The interface does not exist yet: after setup the command says so, lists the subcommands, and exits `2`.

Without setup, `radian arc` and every subcommand except `init` refuse with a line that names `radian arc init` and exit `3`.

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

### `radian arc add plan --slug <SLUG> --title <TITLE>`

Reads the request text from stdin to end of input, writes `plans/<id>.md`, and prints the id.

| Flag | Value | What it does |
|---|---|---|
| `--slug` | `SLUG` | The part of the id after the number: 1 to 48 characters from `a-z`, `0-9`, `-`, with no leading or trailing `-`. Required. |
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
Each task object holds `id`, `plan`, `title`, `state`, `phase`, `attempt`, `attempt_id`, `token`, `cost_usd`, `after`, `supersedes`, `allow_noop`, `extra_stages`, `max_attempts`, `max_cost_usd`, `max_wall_time_secs`, `stall_timeout_secs`, `base_sha`, `head_sha`, `verified_sha`, `retry_at`, and `changed_outside_arc`.

## `radian arc show <ID> [--json]`

Prints one plan or task: every field, whether it changed outside arc, then its body.

The id is looked up among the plans first, then among the tasks.
A plan and a task can carry the same `NNNN-slug` id because they count in separate sequences, and `show` then prints the plan; `list --plan <id>` always reads the plan.

| Flag | Value | What it does |
|---|---|---|
| `--json` | — | Print one JSON object instead of fields. |

The object holds the same keys as in `list --json`, plus `body`.
An unset field is `null`.
An id that names nothing exits `1`.

## `radian arc adopt <ID>`

Every file arc writes is recorded with a content hash in the ledger.
When a plan or task file on disk is not the one arc last wrote, arc does not act on it: the change may have come from an editor, a pull, or a checkout.

`adopt` records the current content as arc's own, so the item can be used again.
It accepts a change to the body, the title, `after`, `supersedes`, `allow_noop`, `extra_stages`, the per-task limits, and a plan's limits and `tasks` list.

It refuses with exit `3` when the outside change touched a field only arc (for a task) or the owner (for a plan) sets:

- a task: `state`, `phase`, `attempt`, `attempt_id`, `token`, `cost_usd`, `retry_at`, `base_sha`, `head_sha`, `verified_sha`;
- a plan: `status`, `auto`.

A file that already matches the ledger is a no-op and prints so.
Without a recorded version to compare against, `adopt` refuses: it cannot tell which fields changed.

## `radian arc confirm <PLAN>`

`confirm` moves a plan from `draft` to `confirmed`.
A confirmed plan gets no new tasks, and its request text can only gain change records (below).
A plan's `auto` level may only be other than `off` once it is confirmed.

| Argument | Value | What it does |
|---|---|---|
| `<PLAN>` | `ID` | The plan id. Required. |

It refuses with exit `3` when the plan has no tasks, when it is already confirmed, and when its file changed outside arc.
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

| Level | What arc would do | Where it would stop |
|---|---|---|
| `off` | Nothing by itself. You start and merge each task. | Each step |
| `run` | Start each task when its `after` tasks are merged, and run the gate. | `review.awaiting_human` |
| `merge` | `run`, and merge when every required stage and the integration passed. | Failure, budget, or conflict |

This build has no scheduling loop, so `run` and `merge` are stored and no task starts from them yet.
`off` is accepted on any plan; `run` and `merge` need a `confirmed` plan and otherwise exit `3`.
A value outside the set is a usage error (`2`).

## `radian arc pause <PLAN>`

`pause` sets the plan's `auto` level to `off`.

| Argument | Value | What it does |
|---|---|---|
| `<PLAN>` | `ID` | The plan id. Required. |

This build has no scheduling loop, so nothing is running: `pause` only writes the level.
A task that is already started keeps its state, and a later loop reads the level before it starts anything.
It exits `0` on a plan that is already `off`.

## `radian arc start <TASK>`

`start` runs one attempt of one task and waits until it ends.
It is how v0 runs a task: there is no scheduling loop yet, so the owner starts one task at a time.

```text
radian arc start <TASK>
```

| Argument | Value | What it does |
|---|---|---|
| `<TASK>` | `ID` | The task id. Required. |

Before it claims anything it reconciles every task in `doing` (see below) and moves a `todo.retry_wait` task whose `retry_at` has passed back to `todo.ready`.

A claim is refused with exit `3` and changes nothing when:

- the task is not `todo.ready`;
- its plan is not `confirmed`;
- an `after` task is not `done`;
- `[limits] concurrency` tasks are already in `doing`.

The claim is one transaction: the task's `attempt` goes up by one, `attempt_id` becomes `<task>-<attempt>`, a random `token` is written, and the task moves to `doing.starting`.
arc then runs the hidden `radian arc work` wrapper as a child, with the brief on its stdin; the wrapper moves the task to `doing.running`.

While the wrapper runs, arc watches `runs/<task>/attempts/<attempt>/result.jsonl`:

- no new line for the task's `stall_timeout_secs` (else `[limits] stall_timeout_secs`) sends SIGTERM to the wrapper;
- `max_wall_time_secs` (else `[limits] max_wall_time_secs`) since the attempt started does the same;
- after `[limits] cancel_grace_secs` arc sends SIGKILL.

The wrapper forwards SIGTERM to its worker, waits for it, and still writes its `AttemptFinished` record.
A limit of `0` means no limit for that timer.

When the attempt ends, arc applies the result in one transaction (the table below) and prints one line:

```text
<attempt id> <outcome> -> <state>.<phase>
<attempt id> <outcome> -> <state>.<phase> (retry at <UTC time>)
<attempt id> <outcome> -> <state>.<phase> (<reason>)
```

For example `0001-work-1 completed -> review.verifying`.
A result whose token is not the task's current attempt prints `<attempt id> ignored: <why>` and changes nothing.
Before its own line, `start` prints `reconciled: <line>` for each task the reconcile step changed.

`start` exits `0` when the task left `doing`, `1` when it is still in `doing` after the attempt (or on an internal error), `3` when the claim is refused, and `4` when another process holds `state.lock`.

## `radian arc cancel <TASK>`

`cancel` stops one task.
It reconciles `doing` tasks first, as `start` does.

| Argument | Value | What it does |
|---|---|---|
| `<TASK>` | `ID` | The task id. Required. |

| The task is | What `cancel` does |
|---|---|
| `todo` (any phase) | moves it to `failed.cancelled` |
| `doing`, starting or running | SIGTERM to the wrapper, then SIGKILL after `[limits] cancel_grace_secs`; when the wrapper's `AttemptFinished` arrives the task goes to `failed.cancelled`; when it never arrives the task goes to `doing.reconciling` |
| `doing.reconciling` | checks the wrapper's pid and start time; signals it if it is still that process; moves the task to `failed.cancelled` |
| `review`, `done`, `failed` | refused with exit `3` |

The wrapper's pid and its start time both come from the attempt record, so a pid the operating system reused is never signalled.
`cancel` prints `<task> <state>.<phase>`, for example `0001-work failed.cancelled`.
It exits `0` when it stopped the task or left it `doing.reconciling`, `3` for a refused state, and `4` when another process holds `state.lock`.

## `radian arc reset-budget <ID>`

`reset-budget` returns budget-paused tasks to `todo.ready` and resets the counted cost to zero from now on.
The id is looked up among the plans first, then the tasks, as `show` does.

| Argument | Value | What it does |
|---|---|---|
| `<ID>` | a plan or task id | Required. A plan id resets every paused task of that plan. |

- Task form: the task must be `todo.paused_budget`; it moves to `todo.ready` and its `cost_usd` becomes `0`. Any other state is refused (`3`).
- Plan form: every task of the plan that is `todo.paused_budget` moves to `todo.ready`; every task of the plan gets `cost_usd` `0`, because the plan's budget counts all of them. The plan's `auto` level is not changed.

It prints `<id>: <n> task(s) ready` and exits `0`.
An id that names no plan or task exits `1`.

## `radian arc mcp`

`mcp` serves the lead's tools over standard input and output, and exits `0` when its client closes the connection.
A client that closes before the handshake finishes exits `1` and says which handshake request was cut off.
Like every subcommand except `init` it refuses without setup (exit `3`) and names `radian arc init`.

The server speaks MCP over stdio and is meant to be started by a lead, not by hand.
It starts in the working directory it is given, so register it from inside the repository (or with that directory as its working directory): arc reads the repository that contains the current directory.

Register it with Claude Code:

```sh
claude mcp add arc -- radian arc mcp
```

Or add it to a project `.mcp.json`, in the shared form every MCP client reads:

```json
{
  "mcpServers": {
    "arc": {"command": "radian", "args": ["arc", "mcp"]}
  }
}
```

Radian's own form is the same server with an `id`:

```json
{
  "servers": [
    {"id": "arc", "command": "radian", "args": ["arc", "mcp"]}
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

## `config.toml`

`init` writes `config.toml` and every other subcommand reads it.
Unknown keys are refused by name, and `schema_version` must be `1`.
A key you leave out takes its default.

| Key | Type | Default | Allowed values | When it takes effect |
|---|---|---|---|---|
| `schema_version` | integer | `1` | `1` | Every run; another value is refused. |
| `repository.remote` | string | `"origin"` | A remote name that exists in the repository | When arc reads the main branch or pushes a task branch. |
| `repository.main_branch` | string | `"main"` (`init` writes what it found) | A branch name | When a task starts from main, and when main is checked before a merge. |
| `limits.concurrency` | integer | `1` | 1 or more | When a scheduling loop starts a task. |
| `limits.max_attempts` | integer | `3` | 1 or more | When a task fails and arc decides whether to retry. |
| `limits.stall_timeout_secs` | integer | `900` | seconds | When a worker prints no new line for that long. |
| `limits.max_wall_time_secs` | integer | `7200` | seconds | As an absolute limit on one attempt, on top of the stall timeout. |
| `limits.max_cost_usd` | number | not set: no limit | 0 or more, US dollars | When an attempt ends and its cost is counted. |
| `limits.cancel_grace_secs` | integer | `10` | seconds | After a cancel, before the worker gets SIGKILL. |
| `limits.max_auto_rework` | integer | `2` | 0 or more | When an automatic rework would go past the limit; the plan then pauses. |
| `worker.exec_args` | array of strings | `[]` | any `radian exec` argument | Every attempt: the arguments go to the worker after the wrapper's fixed ones. Empty means the worker uses the host's normal model configuration. |
| `gate.profile` | string | `"default"` | The name of one `[gate.profiles.<name>]` section | When a gate runs. |
| `gate.command_timeout_secs` | integer | `3600` | seconds | Per gate command line. |
| `gate.profiles` | table | one `default` section | One section per profile name | When a gate runs. |
| `gate.profiles.<name>.stages` | array of tables | `[{ required = true, stage = "commands" }, { required = true, stage = "human" }]` | `stage` is `commands`, `mutation`, `review`, `ci`, or `human`; `required` is `true` or `false` and defaults to `true` | When a gate runs, in order. `integration` is not a profile stage: it always runs at merge. |
| `gate.profiles.<name>.commands` | array of strings | `[]` | Shell command lines | The `commands` stage runs them in order on its own checkout. |
| `gate.profiles.<name>.key_commands` | array of strings | `[]` | Shell command lines | The integration stage runs them on the merge candidate. |

`gate.profiles.<name>` above is written out as `gate.profiles.default` in the file `init` writes.
A stage may be listed once per profile: a profile that lists a stage twice, or that lists `integration`, is refused by name.
A per-task field of the same name overrides a `[limits]` value for that task; a plan budget adds to the task budgets.

## Plan and task files

A plan file is `plans/<id>.md`; a task file is `tasks/<id>.md`.
Both are front matter between two `---` lines, then a body.
arc is the only writer; you change them with the subcommands above, and `adopt` after a hand edit.

An id is a four-digit number, a dash, and a slug (`0001-first`).
Plans count in one sequence and tasks in another, so a plan and a task can share an id.
The number is the next free one and is never reused while its ledger entry exists; the id is the file name and never changes.

### Plan fields

| Field | Type | Required | Allowed values |
|---|---|---|---|
| `schema_version` | integer | yes | `1` |
| `id` | string | yes | `NNNN-<slug>` |
| `title` | string | yes | any text |
| `status` | string | yes | `draft`, `confirmed` |
| `auto` | string | yes | `off`, `run`, `merge`; only a `confirmed` plan may use `run` or `merge` |
| `max_cost_usd` | number | no | 0 or more, US dollars |
| `max_wall_time_secs` | integer | no | seconds |
| `tasks` | list of ids | yes | the plan's tasks, in display order (`[]` when there are none) |

The body is the request text, then a `## Change records` section.
Each change record is a `### <UTC time>` line (the shape `YYYY-MM-DDTHH:MM:SSZ`), a blank line, and the text of the change.
A confirmed plan gains records only; a draft plan may have its request text replaced, which keeps the records below it.

### Task fields

| Field | Type | Required | Allowed values |
|---|---|---|---|
| `schema_version` | integer | yes | `1` |
| `id` | string | yes | `NNNN-<slug>` |
| `plan` | string | yes | the plan's id |
| `title` | string | yes | any text |
| `state` | string | yes | `todo`, `doing`, `review`, `done`, `failed` |
| `phase` | string | yes | one of the phases of the task's state, in the table below |
| `after` | list of ids | yes | tasks that must be merged first (`[]` when there are none) |
| `supersedes` | string | no | the id of a task this one replaces |
| `attempt` | integer | yes | 0 or more; `0` when the task is new |
| `attempt_id` | string | no | the current attempt's `<task>-<n>`, written by the claim |
| `token` | string | no | the current attempt's random token; a result is applied only when it matches |
| `cost_usd` | number | no | the task's counted cost in US dollars, summed over its attempts; `reset-budget` makes it `0` |
| `allow_noop` | boolean | yes | `true`, `false`; `false` when the task is new |
| `extra_stages` | list of strings | yes | gate stages added to the profile (`[]` when there are none) |
| `max_attempts` | integer | no | 1 or more; overrides `limits.max_attempts` for this task |
| `max_cost_usd` | number | no | 0 or more, US dollars; overrides the plan limit |
| `max_wall_time_secs` | integer | no | seconds; overrides the plan limit |
| `stall_timeout_secs` | integer | no | seconds; overrides `limits.stall_timeout_secs` |
| `base_sha` | string | no | the commit the task branch started from; filled by a later build |
| `head_sha` | string | no | the task branch's tip; filled by a later build |
| `verified_sha` | string | no | the commit the gate verified; filled by a later build |
| `retry_at` | string | no | an absolute UTC time, `YYYY-MM-DDTHH:MM:SSZ` |

The body is the brief.

### States and phases

| State | Phases |
|---|---|
| `todo` | `ready`, `blocked`, `retry_wait`, `paused_budget` |
| `doing` | `starting`, `running`, `reconciling` |
| `review` | `verifying`, `rework_pending`, `awaiting_human`, `integrating`, `ready_to_merge` |
| `done` | `merged` |
| `failed` | `exhausted`, `cancelled`, `irrecoverable` |

A new task is `todo` / `ready`.
A plan is done when all of its tasks are merged; `done` is not a stored plan status.

## Failures, retries and the budget

One attempt's result maps to one class and one next state.
The class is recorded in `runs/<task>/attempts/<attempt>/attempt_result.json`.

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
| `stall` (arc sent SIGTERM) | `stall` | retry |
| `wall_time` (arc sent SIGTERM) | `wall_time` | retry |
| `cancelled` by `radian arc cancel` | `cancel` | `failed.cancelled` (never retries) |

A retry moves the task to `todo.retry_wait` with `retry_at` = now + a backoff: 10 seconds for the first attempt, doubled for each later attempt, capped at 5 minutes.
When `attempt` has reached the task's `max_attempts` (else `[limits] max_attempts`), the task goes to `failed.exhausted` instead.
`todo.retry_wait` becomes `todo.ready` when `retry_at` has passed; `start` does this before it claims.

Cost is counted per attempt.
`Result.cost` is the session total, so the attempt's cost is this attempt's session total minus the previous attempt's.
The delta is added to the task's `cost_usd`; the attempt result records `cost_usd` and `session_cost_usd`.

When a retry is due and the budget is used up, the task goes to `todo.paused_budget` instead of `todo.retry_wait`:

- the task's `max_cost_usd`, else `[limits] max_cost_usd`;
- the plan's `max_cost_usd`, counted over every task of the plan. A plan budget also sets the plan's `auto` to `off`.

`reset-budget` is the only way back.

## Reconcile

arc keeps no attempt state in memory.
Before `start` or `cancel` acts, and after a restart, it reconciles every task in `doing`:

- the wrapper's `attempt_finished.json` is present and its token is current: the result is applied;
- the wrapper process (its pid and start time) is alive, or the `start` process that claimed the attempt is: the task is left alone;
- neither is alive and there is no result: the attempt is an `infrastructure` (terminated) failure and the retry rules above apply.

Reconcile never starts a task again.
`doing.reconciling` is left for you to resolve; `radian arc cancel` moves it to `failed.cancelled` after the same process check.

## The `.radian/arc/` layout

Everything arc stores for one repository is in `<repository>/.radian/arc/`:

| Path | What it holds |
|---|---|
| `config.toml` | The settings above. |
| `plans/<id>.md` | One plan: its fields and the request text. |
| `tasks/<id>.md` | One task: its fields and the brief. |
| `ledger/plans/<id>.json` | The `state_version` and SHA-256 arc last wrote for that plan. |
| `ledger/plans/<id>.last.md` | The plan as arc last wrote it, for `adopt`. |
| `ledger/tasks/<id>.json` | The same for a task. |
| `ledger/tasks/<id>.last.md` | The task as arc last wrote it. |
| `runs/<task>/…` | One attempt's output: the worker's `result.jsonl`, `stderr.log`, `execution.json`, the wrapper's `attempt_finished.json`, `signal.json` when arc signalled it, and arc's `attempt_result.json`. |
| `attempts/<task>/…` | The host-local attempt records: `base_sha`, `last_head`, and per attempt `start.json`, `identity.json`, `status.json`. |
| `gates/<stage>/` | One checkout per gate stage (the gate library uses it; no subcommand runs the gate yet). |
| `worktrees/<task>/` | The worker's checkout for that task (the worker wrapper creates it). |
| `state.lock` | The short lock every change takes. |
| `runner.lock` | The lock a scheduling loop holds. |

Every change is one transaction under `state.lock`:
take the lock (waiting up to 5 seconds, then exit `4`), compare the file with the ledger, write a temporary file, flush it to disk, rename it over the old one, flush the directory, record the new version and hash in the ledger, release.

Search skips `worktrees/`: `grep`, `glob`, skill discovery, and `@` completion do not enter it.
They skipped `.radian-worktrees/` for the same reason.
A worker never reads the rest of `.radian/arc/` from inside its checkout: the worker wrapper denies every entry except `worktrees/`.

## Not built yet

These subcommands are named in the design and do not exist in this build.
Do not rely on their flags: they have none yet.

`merge`, `reject`, and `run`.
The arc terminal interface is not built either.
The scheduling loop that would call the gate and start tasks in the background is not built: `start` runs one task at a time, and `reconcile` is the restart step that loop will call.

## See also

- `cli` for every other `radian` command.
- `environment` for the files radian reads, including this one.
