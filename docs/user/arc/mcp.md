---
description: The lead's MCP server: registering it, its tools and its tool errors.
---

# arc: the MCP server

This page covers `radian arc mcp`; the `radian arc` commands are in [`arc`](index.md), the scheduling loop is in [`arc/loop`](loop.md), the gate and its failures are in [`arc/gate`](gate.md), and the files arc keeps are in [`arc/files`](files.md).

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

### The claude-experimental flag

`radian arc mcp --claude-experimental` turns the server into a channel: a client that understands Claude Code's channel extension starts a turn with each task transition it receives.
The flag is a boolean, and it is off by default; without it the server is exactly the server on this page: the same tools, the same capabilities, the same instructions, and no notification is ever sent.

With the flag the `initialize` answer carries `capabilities.experimental["claude/channel"] = {}`.
It does not declare `claude/channel/permission`: the server never relays permission prompts.

The supported protocol versions are the known versions up to and including `2025-11-25`.
A `server/discover` request is answered with the unsupported-protocol-version error, and an `initialize` that asks for `2026-07-28` is answered with `2025-11-25`.
That is what makes Claude Code register the server as a channel: a revision at or after `2026-07-28` cannot carry channel messages.

The server reads every plan and its tasks once after the handshake.
That read is the baseline and sends nothing, so a task that already is `review.awaiting_human` when the session opens is not reported, and neither is a plan that appears already confirmed.
It then reads every plan and its tasks again every 5 seconds, with the read-only store call `list` uses, and takes no lock.
A read that fails (a file changed outside arc, a parse error, a lock it cannot get) sends nothing that round and tries again next round; it never ends the server and prints nothing.

One event is sent per task whose `state.phase` differs from the previous read and whose new value is one of these:

`todo.proposed`, `review.awaiting_human`, `review.rework_pending`, `done.merged`, `failed.exhausted`, `failed.cancelled`, `failed.irrecoverable`.

A move into `todo.ready` sends one too, from one of these previous phases: `todo.proposed` (your `radian arc confirm <task>`) and `review.*` (your reject, or the lead's `add_rework`; the poller cannot tell the two apart, and both are worth a turn).
A reject keeps the task in `review.rework_pending`, so its event is that one and its second line is the note (see [`reject`](index.md#radian-arc-reject-task---note-text)).
A rework arc writes itself (a conflict with main, or a review that failed with blocking items) is not your action: its rework heading carries a suffix and the move sends nothing.

A task that is new since the previous read counts as a transition from nothing.
Every other change (`doing.*`, `review.verifying`, `review.integrating`, a `todo.retry_wait` or `todo.paused_budget` task that became `todo.ready`, a title edit) sends nothing: each event costs the lead a model turn.

A plan sends one event per field whose value differs from the previous read, over these fields: `status` and `paused`.

- `status` is sent only for `draft` to `confirmed`. A plan that returns to `draft` with a change record sends nothing.
- `paused` is `true` when the plan's `auto` is `off`, and the event is sent in both directions. `radian arc pause` and the automatic rework limit (`auto` goes to `off`) send the pause; the next level that is not `off` sends the resume. `auto` is the only field `list --json` exposes for this, so a level change from `run` to `merge` sends nothing, and so does a change of `priority` or a title.
- A plan that is new since the previous read sends nothing for either field, and a cancelled plan sends no plan event at all: it is final.

The notification's method is `notifications/claude/channel`.
A task event's `content` is one line `<task id> <previous state.phase or "new"> -> <state.phase>`, then, when the task has a `rework_reason` (the same text `show --json` gives, a reject note included), a second line with it.
A task event's `meta` has exactly these keys, and every value is a string:

| Key | Value |
|---|---|
| `task` | the task id |
| `plan` | the plan id |
| `state` | the new `state` |
| `phase` | the new `phase` |
| `previous` | the previous `state.phase`, or `new` |

A plan event's `content` is one line `<plan id> <field> <previous> -> <value>`, for example `PL0044-x status draft -> confirmed` or `PL0044-x paused false -> true`, then, when the store records a pause reason (the automatic rework limit's text), a second line with it.
A plan event's `meta` has exactly these keys, and every value is a string:

| Key | Value |
|---|---|
| `plan` | the plan id |
| `field` | `status` or `paused` |
| `previous` | the previous value: the status text, or `false`/`true` for `paused` |
| `value` | the new value |

A plan event has no `task` key, so a reader tells it from a task event.
A meta key stays letters and underscores; Claude Code drops a key with any other character.
The channel is one-way: there is no tool to reply to an event, and there is still no tool to confirm, start, merge, reject, approve, or change the auto level.
The poller stops when the client closes the connection.

The channel is the stdio connection to the lead's own process: no network and no new telemetry.

Load it in an interactive Claude Code session with

```sh
claude --dangerously-load-development-channels server:arc
```

The name after `server:` is the name the server is registered under (here `arc`, from the registration above).
A plain `--channels` accepts only Claude Code's own list of channel plugins, so the server's own name does not load through it.
Claude Code ignores the flag in a non-interactive `-p` session, so the events reach only an interactive session.

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

There is no tool to confirm a plan or a task, start a task, merge, reject, change `auto`, or approve: those are yours (see [`confirm`](index.md#radian-arc-confirm-id), [`auto`](index.md#radian-arc-auto-plan-level), and [`pause`](index.md#radian-arc-pause-plan)).

`add_plan` creates a `draft` plan with `auto` `off`.
The `slug` rule and the title rule are the ones in [`radian arc add`](index.md#radian-arc-add); an empty title is refused.

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
The arc exit codes belong to the CLI (see [`arc`](index.md#exit-codes)); a tool error carries no code.
An id that names both a plan and a task is a tool error that names both files (see [Plan and task files](files.md#plan-and-task-files)); the CLI exits `3` for it.
