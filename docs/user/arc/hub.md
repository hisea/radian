---
description: The hub: its HTTP API on loopback, pairing and the bearer token, every route, the repository registry, the scheduling loop, and the `[hub]` configuration keys.
---

# hub

A hub is one process that manages every repository registered on this machine.
[`radian hub`](#radian-hub) serves the HTTP API on loopback and runs the scheduling loop for every registered repository; `radian hub add`, `remove` and `list` manage the registry; [`radian hub pair`](#radian-hub-pair) and [`radian hub token rotate`](#radian-hub-token-rotate) handle the bearer token every request carries.

## The registry file

`$RADIAN_HOME/hub/repos.toml` (default `~/.radian/hub/repos.toml`) lists the repositories.
The hub directory is `0700` and the file is `0600`.
A missing file is an empty registry.

```toml
[[repos]]
name = "radian"
path = "/home/me/Code/radian"

[[repos]]
name = "work-tools"
path = "/home/me/Code/work-tools"
```

| Key | Type | Rule |
|---|---|---|
| `name` | string | 1 to 48 characters from `a-z`, `0-9` and `-`; unique. |
| `path` | string | an absolute, canonical path to a directory with a `.radian/arc/`; unique. |

Entries are listed in file order.
Every write goes through a temporary file and an atomic rename, so a process that re-reads the file sees either the old document or the new one, never a partial file.
A file that does not parse, a name outside its rule, a path that is not an arc repository, and a duplicate name or path are errors that name the file and the line, or the path.

## `radian hub`

`radian hub` with no subcommand starts the hub and runs until the process is stopped.
It binds `127.0.0.1:<hub.port>` and only that address; there is no key for another address and no key that turns the listener off.
The bind happens after it takes `$RADIAN_HOME/hub/hub.lock`, a lock that holds the process's pid and lives as long as the process: a second `radian hub` exits `1` with a line that names `hub.lock` and the holder's pid, before it binds anything or touches any repository.
A port another program holds exits `1` with a line that names the port.

Its first stdout line is `hub: listening on 127.0.0.1:<port>`.
The scheduling loop then runs in the same process; [the loop](#the-loop) describes it.

### `radian hub add <PATH> [--name <NAME>]`

Registers the repository at `PATH`: a directory that has a `.radian/arc/`.
`PATH` is canonicalized first, and the registry stores the canonical path.
Without `--name`, the name is the directory's base name.
A name outside the rule, a duplicate name, a duplicate path (a symlink to an already registered repository included), and a path that is not an arc repository are refused, and the file does not change.
It prints `added <name>  <path>`.

| Flag | Value | Default | What it does |
|---|---|---|---|
| `--name` | `NAME` | the directory's base name | The name to register. 1 to 48 characters from `a-z`, `0-9` and `-`. |

### `radian hub remove <NAME>`

Removes the repository named `NAME` and prints `removed <name>`.
An unknown name is refused and nothing changes.

### `radian hub list [--json]`

Prints one line per repository, in file order: `name  path` (two spaces).
With `--json`, it prints one JSON array of `{ "name": ..., "path": ... }`.

| Flag | Value | Default | What it does |
|---|---|---|---|
| `--json` | a flag | off | Print the JSON array instead of one line per repository. |

### `radian hub pair`

Prints the URL `http://127.0.0.1:<hub.port>`, then the token, then (on a terminal, unless `--no-qr`) a QR code of `radian://hub?url=<url>&token=<token>` drawn with Unicode half blocks.
A pipe or a file gets the URL and the token only: the QR code is printed only when stdout is a terminal.
It creates the token when the file is missing, so a client can pair before the hub has ever run.

| Flag | Value | Default | What it does |
|---|---|---|---|
| `--no-qr` | a flag | off | Print the URL and the token only, even on a terminal. |

### `radian hub token rotate`

Writes a new token and prints one line, `rotated`.
The old token fails on the next request, with no restart: the running hub checks the token file's modification time on every request and re-reads it when it changed.

### Exit codes

| Code | Meaning |
|---|---|
| `0` | success |
| `1` | a registry or startup error: the file does not parse or breaks a rule, the path is not an arc repository, the name is unknown or taken, another hub holds `hub.lock`, or the port is in use |
| `2` | a usage error: a missing argument, an unknown flag, or a `--name` outside the rule |

A closed output pipe (`radian hub list | head`) is a normal stop: exit `0`.

## The token

`$RADIAN_HOME/hub/token` (default `~/.radian/hub/token`) holds the bearer token.
The hub directory is `0700` and the file is `0600`; the sandbox cannot read either.
The file holds 32 random bytes as 64 lowercase hex characters.
`radian hub` creates it on its first start when it is missing, and so does `radian hub pair`.

Every request must carry `Authorization: Bearer <token>`.
A missing header, a malformed one, a wrong token, and a token under any scheme other than `Bearer` (for example `Basic`) are all `401` with the body `{}`.
There is no way to turn the token off.
A Tailscale identity header is not a credential.

The token never appears in a log line, and a request the hub refuses changes no file and no state.
`radian hub token rotate` writes a new token through a temporary file and an atomic rename, so a reader sees either the old token or the new one.
A running hub stat()s the file once per request and re-reads it when its modification time changed, so the new token works and the old one fails on the next request.

## Version

The API has two version numbers.

- `api_version` names the route set and its behavior. This build's is `1`, and every route starts with `/v1/`.
- `schema_version` names the plan and task file shape (see [the file fields](files.md)). This build's is `1`.

They change on their own: a route can be added without touching a plan or task file, and a field can be added to a file while `api_version` stays `1`.
`GET /v1/version` returns both.

## Routes

The read routes and the event stream take `GET`; the owner actions take `POST` (see [Owner actions](#owner-actions)).
Every body is `application/json` except [the event stream](#the-event-stream), which is `text/event-stream`; a `204` has no body.
The list and show bodies are exactly the bytes `radian arc list --json` and `radian arc show --json` print in that repository, so a field the CLI gains appears on the route too.

A request without a valid bearer token is `401` with `{}` before any route runs.
The token is checked before the request body is read, so a request without one is answered without the hub buffering the body it sends.
The route and the method are checked before the body is read too: an unknown path is `404` and a wrong method is `405` whatever body the request carries, and only a write route's body is read at all.
An unknown repository or id on a read route is `404` with the CLI's own message as `{"error": "<message>"}`.
Any other read path is `404` with `{"error": "no such route"}`, and a method a read route does not take is `405`.
A write route's failure body and status codes are in [Owner actions](#owner-actions).

| Route | Method | Fields | Status codes |
|---|---|---|---|
| `/v1/version` | `GET` | `api_version` (integer), `schema_version` (integer). | `200`, `401` |
| `/v1/repos` | `GET` | One array in registry order; each entry has `name` (string), `path` (string), `scheduled` (boolean), and `lock_holder`. `scheduled` is `true` when this hub holds the repository's `runner.lock`. `lock_holder` is the pid written in that `runner.lock` while a process holds it (the hub's own when it schedules the repository, another process's when it only reads it), or `null` when no process holds it or the file carries no readable pid. Both fields follow a probe of the lock, not the pid text left in the file after its holder exits. | `200`, `401` |
| `/v1/repos/<name>/items` | `GET` | The `radian arc list --json` bytes of that repository. | `200`, `401`, `404` |
| `/v1/repos/<name>/items/<id>` | `GET` | The `radian arc show --json` bytes of that plan or task. | `200`, `401`, `404` |
| `/v1/repos/<name>/items/<task>/report` | `GET` | `attempt` (string, `<task>-<n>`), `text` (string): the last assistant message of the newest attempt's `result.jsonl`. | `200`, `204` when there is no attempt or no message, `401`, `404` |
| `/v1/repos/<name>/items/<task>/verdicts` | `GET` | One array for the newest attempt, in the order `commands`, `mutation`, `review`, `ci`, `human`, `integration`; each entry has `stage` (string), `result` (`pass` or `fail`), `reason` (string or `null`), `path` (string, the verdict file), and `verdict` (the verdict's JSON). | `200`, `401`, `404` |
| `/v1/repos/<name>/items/<task>/diff` | `GET` | One array of `path` (string), `added` (integer) and `removed` (integer), from `git diff --numstat` for the task's own change; a binary file reports `0` and `0`. | `200`, `401`, `404` |
| `/v1/repos/<name>/items/<task>/diff/<path>` | `GET` | `path` (string), `truncated` (boolean), `text` (string): that file's unified diff, cut at 256 KiB (262,144 bytes) with `truncated` `true`. A path not in the list is `404`, and so is a git that cannot produce the diff: a failed read is never a `200` with an empty text. | `200`, `401`, `404` |
| `/v1/events` | `GET` | A server-sent-event stream of every state change; see [the event stream](#the-event-stream). | `200`, `401` |
| `/v1/repos/<name>/items/<id>/<action>` | `POST` | An owner action; see [Owner actions](#owner-actions). | `200`, `400`, `401`, `404`, `405`, `409`, `413`, `422`, `500` |

`<path>` in the last read route is percent-encoded, so a file whose name contains `/` stays one path.

## Owner actions

`POST /v1/repos/<name>/items/<id>/<action>` is an owner's action in the API: it calls the same library function the arc TUI calls, in the hub's process, and changes the store. It uses only that repository's `state.lock`, never `runner.lock`.

| Action | Body | What it does |
|---|---|---|
| `confirm` | `{}` | Confirms a draft plan or a `todo.proposed` task, as `radian arc confirm`. |
| `merge` | `{}` | Approves and finishes a `review.awaiting_human` task, as `radian arc merge`. |
| `cancel` | `{}` | Cancels a task, or a plan and its open tasks, as `radian arc cancel`. |
| `pause` | `{}` | Sets the plan's `auto` level to `off`, as `radian arc pause`. |
| `reverify` | `{}` | Re-runs the gate without a worker, as `radian arc reverify`. |
| `reject` | `{"note": "<text>"}` | Sends a `review.awaiting_human` task back to rework with the note, as `radian arc reject`. The note must not be empty or whitespace. |
| `auto` | `{"level": "off" \| "run" \| "merge"}` | Sets the plan's `auto` level, as `radian arc auto`. |
| `priority` | `{"priority": "P0" \| "P1" \| "P2" \| "P3" \| "P4" \| "P5"}` | Sets the plan's priority, as `radian arc priority`. `P0` is the highest and `P3` the default. |

A body with no field is enough for `confirm`, `merge`, `cancel`, `pause` and `reverify`; an empty body is the same as `{}`.
A missing or empty `note`, `level` or `priority` is refused, not defaulted.
A request body may be at most 64 KiB (65,536 bytes), and only a write route's body is read: a read request's body is ignored. A body over the limit is never kept: a declared `Content-Length` marks it too large before any buffering, the hub drains at most 1 MiB of what is left for at most 250 ms so a client still sending can read the answer, and the connection closes. It is answered `413` with `{"error": "request body too large"}`; nothing changes.

A successful action answers `200` with `{"ok": true, "text": "<the CLI's stdout text>"}`.
A failed action answers `{"ok": false, "text": "<the CLI's message>"}` and one of these status codes:

| Status | When |
|---|---|
| `400` | `reject` without a note: the CLI's empty-note message, and nothing changes. |
| `404` | The repository is not registered, or the id names no plan or task. |
| `405` | A write route was called with a method other than `POST`. |
| `409` | Another process holds `state.lock`, or the state does not allow the action (for example a merge on a task that is not `review.awaiting_human`). A `reverify` whose claim succeeded and whose verification then refused also answers `409`, with the claim's line and the verification's own error after it. |
| `422` | A usage or validation error: a value outside the action's closed set, or another check the CLI refuses with exit `2` or with exit `3` for a value outside a set. |
| `500` | Any other error, such as a git or file failure. |

The route checks `state.lock` before it acts, so a held lock answers `409` with `state.lock is locked by another process` and changes no file.
It waits no longer than the CLI's own lock wait.

There is no second confirmation in the API: `merge`, `reject`, `auto` at level `merge` and `cancel` act at once.
**The client asks before it calls them.**

`start` and `reset-budget` are not routes.
Any other action name is `404` with `{"error": "no such route"}`, and `GET` on a write route is `405`.

## The event stream

`GET /v1/events` is a server-sent-event stream of every state change in every registered repository.
It needs the bearer token like every route and answers `Content-Type: text/event-stream`.

Each change is one event:

```
event: transition
data: {"repo":"radian","kind":"task","id":"TK0001-x","field":"state","previous":"review.awaiting_human","value":"review.rework_pending"}

```

| Key | Value |
|---|---|
| `repo` | The repository's registry name. |
| `kind` | `task` or `plan`. |
| `id` | The task's or plan's id. |
| `field` | `state` for a task; `status` or `paused` for a plan. |
| `previous` | The old value. |
| `value` | The new value. |

A task event's `previous` and `value` are `<state>.<phase>`; `previous` is `new` for a task the stream's first read did not have.
A plan event carries the same `field`, `previous` and `value` the MCP channel sends.
The changes are the closed list [the MCP channel](mcp.md#the-claude-experimental-flag) sends: every other change sends nothing.

While nothing happens the stream carries one `: keep-alive` comment line every 15 seconds.
An event has no id, and the stream does not replay: a client that reconnects first reads the routes above again, then follows the stream.
The stream ends when the client disconnects or the hub stops.

## Notifications

A lock-screen notification is a [`[hooks]`](hooks.md#hooks) command, not something the hub sends.
The hook runs in the scheduling loop, so the notification is sent whether or not a client is connected.
These two `after_awaiting_human` lines send only the repository name (the directory name), the task id and the phase: no title, no brief and no diff.

ntfy:

```toml
[hooks]
after_awaiting_human = [
  "curl -s -d \"$(basename \"$PWD\") $ARC_TASK $ARC_STATE\" https://ntfy.sh/<your-topic>",
]
```

Bark:

```toml
[hooks]
after_awaiting_human = [
  "curl -s \"https://api.day.app/<device-key>/$(basename \"$PWD\")/$ARC_TASK%20$ARC_STATE\"",
]
```

`after_proposed`, `after_rework` and `after_fail` are the other points a notification usually watches.
The client opens the task over the hub for the title, the brief and the diff.

## The loop

Each registered repository gets one loop.
The loop takes that repository's `runner.lock` and holds it for its life, runs the same pass `radian arc run` runs, waits `hub.interval_secs`, and runs another, until SIGINT or SIGTERM.
It is the same loop for every repository: `radian arc serve` no longer exists, and `radian arc run` is still the one-shot pass.
At start, and every `hub.interval_secs` afterwards, the hub re-reads the registry; a repository added since the last read gets its lock and a pass in the next round, and a removed one releases its lock after its pass in flight.

The loops are OS threads, one per repository, not async tasks on one runtime: a pass is synchronous and blocking (it starts and waits for git and gate processes, and it sleeps between passes), so one slow pass does not delay another repository's pass or an HTTP request.
The hub never writes the registry.

A repository whose `runner.lock` another process holds is not scheduled.
The hub's status line names it `read-only` with the holder's pid (or `read-only (holder unknown)` when the lock file carries none), and probes the lock again every interval.
When the holder lets go, the next probe schedules the repository.

The machine-wide [`hub.concurrency`](#the-hub-configuration) cap applies on top of each repository's own [`limits.concurrency`](config.md#configtoml) and flow concurrency.
Before it starts a task, a pass reads the number of tasks in `doing` in every registered repository at that moment and starts the task only when that number is below the cap.
A repository whose store cannot be read is left out of that count, so a broken task file there cannot stop another repository's pass; that repository's own loop reports the error.
The count is read, not cached: two passes in different repositories can each see a free slot and both start one task, so the cap bounds the steady state, not a lock; the next passes see the new count.

A pass error in one repository is written to that repository's [`loop.log`](files.md), and its loop continues at the next interval; the other repositories are unaffected.
As with the old loop, three passes in a row with the same error text stop that repository's loop: it stops the gates it started the way a signal does (workers are left alone), releases its `runner.lock`, and the hub keeps running and scheduling every other repository.
Restart the hub to schedule that repository again.

The status is one line per repository, printed on start and whenever it changes:

```text
hub: 0 in doing, cap 4
radian  /home/me/Code/radian  scheduled
work-tools  /home/me/Code/work-tools  read-only (pid 4242)
```

A running hub follows a rebuilt binary as the old loop did: when a pass sees that the file at its recorded executable path changed and is stable, the hub prints `radian arc: the radian binary changed on disk; restarting the loop with it` once, in the `loop.log` of the repository whose pass saw it, finishes the passes in flight, releases every `runner.lock` and then `hub.lock`, and replaces itself with `exec` of the recorded path.
A loop that sees the same replacement after the first one stops without printing a second line.
The pid does not change, and a running gate and a running worker are left alone.
When `exec` fails, the hub writes one line to the `loop.log` of the repository whose pass claimed the restart and prints it, takes `hub.lock` again and goes on with the old code.

On SIGINT or SIGTERM every loop starts no new worker and no new gate.
A Ctrl-C in the terminal reaches the hub only: each worker and each gate runs in its own process group, and both treat SIGINT exactly as SIGTERM.
A running worker is left alone: it is a separate process with its own limits and record, and the next pass (or the next `radian arc run`) collects it.
A running gate is signalled and gets `[limits] cancel_grace_secs` before SIGKILL, and so is one whose record is still `started` while it holds the gate lock; the gate forwards the signal to the stage it is running, waits for it, and exits without a record; once it is gone its record becomes `interrupted`, so a gate that did not finish is started again by the next loop as unverified and counts no death, and the verdicts it already wrote are reused, unless it exited before writing any record at all: then the next pass reports it and starts no gate while its inputs are unchanged (see [the gate run](gate.md#the-gate-run)).
Each loop then prints its summary, releases its `runner.lock`, and the hub releases `hub.lock` last and exits `0`.

## The `[hub]` configuration

`[hub]` lives in the user configuration file (`~/.radian/config/config.toml`); see [`configuration`](../configuration.md#hub).
A change takes effect at the hub's next start.

| Key | Type | Default | Allowed | What it does |
|---|---|---|---|---|
| `hub.port` | integer | `7341` | `1` to `65535` | The HTTP API's port. The API binds `127.0.0.1` only. |
| `hub.concurrency` | integer | `4` | `1` or more | The machine-wide cap on tasks in `doing` across every registered repository. |
| `hub.interval_secs` | integer | `30` | `5` to `3600` | Seconds between two registry reads and two scheduling passes of each repository. |

A value outside its range is a configuration error naming the key and the range.
An unknown key under `[hub]` is skipped and named at startup.
