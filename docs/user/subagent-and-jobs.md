---
description: Run helper sessions and background shell jobs.
---

# Helpers and jobs

Two tools do work off the main path: `subagent` runs one helper session,
and `shell` with `background: true` starts a background job that `job`
manages. One helper session runs at a time per call; several calls in one
reply run in parallel.

## The helper session

Use the `subagent` tool to run one helper session. The call blocks until the
helper finishes, and returns the last assistant text. Long text is cut at
4000 characters and marked. The result names the helper session id. The
helper starts with a blank history; the first input is the `task` text, so
tell it what to do and what to report.

| Field | Required | What it does |
|---|---|---|
| `task` | yes | The helper's first input. |
| `agent` | no | `explore` for read-only work, `general` for full work. Omit it (or pass `""`) to run `general`. Unknown values fail the call. |

The `explore` helper mounts only `read`, `glob` and `grep`. The `general`
helper mounts the parent tools minus `subagent`: a helper never mounts
`subagent`, so helpers never nest. An `explore` child also skips the project
MCP sources. `explore` fails when the session has no workspace: it needs the
read tools rooted at one. A parent in automatic mode approves helper questions at once; a
parent outside automatic mode denies them, and the denial is noted in the
helper result.

The helper inherits the parent tier and can only narrow it. A `general`
child runs the parent's live route, including a switch made after startup.
An `explore` child runs `[model.smol]` when the config sets a full cheap
route, else it follows the parent route. A half-set cheap route warns and is
ignored.

## A worked helper example

Ask the model to fan out two read-only searches in one reply:

```json
{"agent": "explore", "task": "List every file that defines a slash command, with one line each on what it registers."}
{"agent": "explore", "task": "List every file that defines a key binding, with one line each on what it binds."}
```

Each call returns its own conclusion. The model then compares the two lists.
For a write task, use one `general` call with an explicit scope and an
explicit report shape:

```json
{"agent": "general", "task": "Add the `usage` row to the commands table in docs/user/commands.md. Report the lines you changed."}
```

## Background jobs

Use the `shell` tool to run commands. A foreground command never moves to
the background on its own: only an explicit `background: true` starts one,
and the call returns at once with a job id. The foreground default timeout
is 300 seconds and the maximum is 600. Use `background: true` for servers,
watchers and commands expected to run longer than 600 seconds. Run tests and
builds in the foreground with `timeout_secs`.

Use the `job` tool to manage background commands. The same registry holds
retained foreground output, so a capped foreground result stays readable
through its id. Old entries leave when the registry fills.

| Operation | What it does |
|---|---|
| `op: read` | Read output. Returns the increment since the last read plus the settle state when done. The read window is 64 KiB per job; the file keeps the full history for the host. |
| `op: wait` | Wait for completion, default 30 seconds, at most 600. Returns the increment and the settle. |
| `op: kill` | Stop the whole session: SIGTERM, then SIGKILL after 3 seconds. |

At most 4 background jobs run at once in one session. A fifth start fails
with an error naming the limit. One approval covers a job's whole life.

Type `/jobs` to open the jobs page. It lists the running background jobs in
registry order. Type `/jobs kill <id>` to stop one job, `/jobs kill all` to
stop every job. The kill signals the job's whole session: `kill <id>` sends
SIGTERM, then SIGKILL after 3 seconds; `kill all` sends SIGKILL at once. A
settle the model has not seen arrives as a system
message: on a busy lane it joins the running turn, on an idle lane it opens
a follow-up turn that wakes it. After three wake-started turns with no user
input, a settle is injected only.

## A worked job example

Start a watcher in the background, then poll it:

```json
{"command": "make watch", "background": true}
```

The call returns a job id at once. Read the increment:

```json
{"op": "read", "id": "job-1"}
```

Wait for it to settle, or stop it:

```json
{"op": "wait", "id": "job-1"}
{"op": "kill", "id": "job-1"}
```

Ask the model through `ask_user` when you own the trade-off between waiting
and killing. The question tool only appears when a human can answer.

## Shell commands

Run `!<command>` to run a command in the tier shell, under the sandbox when
one is enforced. Run `!!<command>` to keep its output out of model context;
the block draws dimmer so the exclusion reads. A second `!` while one runs
is refused, and `esc` cancels the running command.

Every command the `shell` tool runs, foreground or background, and every `!`
or `!!` command, runs as `/bin/sh -c` in its own session, with an empty
stdin and its output captured.
That has three consequences:

- It has no controlling terminal, so a program that needs one fails: an
  editor, a pager, a password prompt on `/dev/tty`, an `isatty` check.
  Pass the program the input it wants (for example `git commit -m`).
- A signal sent to radian's process group never reaches the command.
- A stop (`timeout_secs`, `esc` or `ctrl-c`, `job kill`) signals
  the command's whole session: SIGTERM, then SIGKILL after 3 seconds. A
  session exit sends SIGKILL alone.
  A child that changed its own process group is still reached (GNU `timeout`
  does that); one that starts a new session is not.

Nothing you can set changes this: not a flag, a configuration key, or a tier.
No notice is printed.
In an untrusted project every command also carries the git safety keys; see
[`permissions`](permissions.md) for the keys and values.
