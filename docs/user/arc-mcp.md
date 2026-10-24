---
description: The lead's MCP server: registering it, its tools and its tool errors.
---

# arc: the MCP server

This page covers `radian arc mcp`; the `radian arc` commands are in [`arc`](arc.md), the scheduling loop is in [`arc-loop`](arc-loop.md), the gate and its failures are in [`arc-gate`](arc-gate.md), and the files arc keeps are in [`arc-files`](arc-files.md).

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
| `update_task` | `task` (string, required), `title` (string, optional, non-empty), `brief` (string, optional, non-empty), `after` (list of ids, optional), `allow_noop` (boolean, optional), `extra_stages` (list of stage names, optional) | the task, as `radian arc show <task> --json` |
| `add_rework` | `task` (string, required), `brief` (string, required) | the task, as `radian arc show <task> --json` |
| `list` | `plan` (string, optional: only this plan) | the JSON array `radian arc list --json` prints |
| `show` | `id` (string, required) | the JSON object `radian arc show --json` prints, a task's `waits_on`, `blocks`, `never_runs` and `cycle` included |

There is no tool to confirm a plan or a task, start a task, merge, reject, change `auto`, or approve: those are yours (see [`confirm`](arc.md#radian-arc-confirm-id), [`auto`](arc.md#radian-arc-auto-plan-level), and [`pause`](arc.md#radian-arc-pause-plan)).

`add_plan` creates a `draft` plan with `auto` `off`.
The `slug` rule and the title rule are the ones in [`radian arc add`](arc.md#radian-arc-add); an empty title is refused.

`add_task` depends on the plan's state. On a `draft` plan it keeps the rules of `radian arc add task`, and the new task is `todo.ready`: it is confirmed with the plan. On a `confirmed` plan the new task is `todo.proposed`, and it waits for you to confirm it with `radian arc confirm <task>`; the plan keeps its `status` and `auto`. A `cancelled` plan is refused, and the error names the status. In both cases each `after` id must name a task of the same plan or a task whose state is `done`, and each `extra_stages` name must be a gate stage.

`update_plan` depends on the plan's state:

- on a `draft` plan, `title` and `request` replace the plan's; a `change_record` is refused (`3`);
- on a `confirmed` plan, `title` and `request` are refused (`3`) because the request text is never rewritten, and `change_record` is required: it is appended under the plan's `## Change records` heading with the UTC time, and the plan returns to `draft` with `auto` `off`, keeping its tasks' state. New tasks can be added again.

A call with no field at all is refused: it would change nothing.
A `cancelled` plan is refused by `update_plan`, and the error names the status.

`update_task` changes a task whose state is `todo` (any phase) in a plan that is not `cancelled`.
It takes `title`, `brief`, `after`, `allow_noop` and `extra_stages`, and at least one of them; a call with none is refused.
It does not change the phase: a `todo.ready` task stays `todo.ready` and a `todo.proposed` task stays proposed, and a change to `after` that makes the task wait on a task not merged yet is allowed, because the scheduler already waits for `after`.
The `title`, `after` and `extra_stages` rules are `add_task`'s: a title that is empty is refused, each `after` id must name a task of the same plan or a task whose state is `done`, a task may not wait on itself, and an `after` cycle is refused.
`brief` replaces only the original brief, the text before the first `## Rework <n>` section; every rework section and the change records below it are kept in order.
Every successful call appends one line to the task's `## Changes` section, created the first time it is needed: `- <RFC 3339 UTC> update_task: <the fields the call gave, comma-separated>`, where the fields are `title`, `brief`, `after`, `allow_noop` and `extra_stages` in that order.
Any other state is refused (`3`) with the state in the error; a task in `review` is refused with a line that names `add_rework`.
A task of a `cancelled` plan is refused, and the error names the status; a task file that changed outside arc is refused like every other write.

`add_rework` applies only to a task whose state is `review` (any phase); anything else is refused (`3`).
A task of a `cancelled` plan is refused, and the error names the status.
The brief is appended under `## Rework <n>`, where `n` is the task's `attempt` plus one, and the task moves to `todo.ready`.
It does not change `attempt`: the attempt number belongs to the claim that starts the next run.

### Tool errors

A tool that cannot do its work answers with `isError: true` and one text line: the reason.
That line is the same text the CLI prints for the same refusal, so a plan or task that changed outside arc says `changed outside arc` and names `radian arc adopt`.
An argument of the wrong type, a missing required argument, and an argument the tool does not take are errors that name the argument.
The arc exit codes belong to the CLI (see [`arc`](arc.md#exit-codes)); a tool error carries no code.
An id that names both a plan and a task is a tool error that names both files (see [Plan and task files](arc-files.md#plan-and-task-files)); the CLI exits `3` for it.
