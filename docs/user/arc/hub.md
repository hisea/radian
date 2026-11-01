---
description: The hub: its HTTP API on loopback, pairing and the bearer token, every route, the repository registry, and the `[hub]` configuration keys.
---

# hub

A hub is one process that manages every repository registered on this machine.
[`radian hub`](#radian-hub) serves the HTTP API on loopback; `radian hub add`, `remove` and `list` manage the registry; [`radian hub pair`](#radian-hub-pair) and [`radian hub token rotate`](#radian-hub-token-rotate) handle the bearer token every request carries.
The scheduling loop is not built in this build: `radian hub` says so and serves HTTP.

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

`radian hub` with no subcommand starts the HTTP API and serves it until the process is stopped.
It binds `127.0.0.1:<hub.port>` and only that address; there is no key for another address and no key that turns the listener off.
The bind happens after it takes `$RADIAN_HOME/hub/hub.lock`, a lock that holds the process's pid and lives as long as the process: a second `radian hub` exits `1` with a line that names `hub.lock` and the holder's pid, before it binds anything.
A port another program holds exits `1` with a line that names the port.

Its first stdout line is `hub: listening on 127.0.0.1:<port>`.
Until the scheduling loop is built, it prints one stderr line, `hub: the scheduling loop is not built yet`, and serves HTTP.

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

The read routes take `GET`; the owner actions take `POST` (see [Owner actions](#owner-actions)).
Every response body is `application/json`; a `204` has no body.
The list and show bodies are exactly the bytes `radian arc list --json` and `radian arc show --json` print in that repository, so a field the CLI gains appears on the route too.

A request without a valid bearer token is `401` with `{}` before any route runs.
An unknown repository or id on a read route is `404` with the CLI's own message as `{"error": "<message>"}`.
Any other read path is `404` with `{"error": "no such route"}`, and a method a read route does not take is `405`.
A write route's failure body and status codes are in [Owner actions](#owner-actions).

| Route | Method | Fields | Status codes |
|---|---|---|---|
| `/v1/version` | `GET` | `api_version` (integer), `schema_version` (integer). | `200`, `401` |
| `/v1/repos` | `GET` | One array in registry order; each entry has `name` (string), `path` (string), `scheduled` (boolean), and `lock_holder`. `scheduled` is `false` until the scheduling loop is built. `lock_holder` is `"hub"` when this hub holds the repository's scheduling lock, the pid written in its `runner.lock` when another process does, or `null`. | `200`, `401` |
| `/v1/repos/<name>/items` | `GET` | The `radian arc list --json` bytes of that repository. | `200`, `401`, `404` |
| `/v1/repos/<name>/items/<id>` | `GET` | The `radian arc show --json` bytes of that plan or task. | `200`, `401`, `404` |
| `/v1/repos/<name>/items/<task>/report` | `GET` | `attempt` (string, `<task>-<n>`), `text` (string): the last assistant message of the newest attempt's `result.jsonl`. | `200`, `204` when there is no attempt or no message, `401`, `404` |
| `/v1/repos/<name>/items/<task>/verdicts` | `GET` | One array for the newest attempt, in the order `commands`, `mutation`, `review`, `ci`, `human`, `integration`; each entry has `stage` (string), `result` (`pass` or `fail`), `reason` (string or `null`), `path` (string, the verdict file), and `verdict` (the verdict's JSON). | `200`, `401`, `404` |
| `/v1/repos/<name>/items/<task>/diff` | `GET` | One array of `path` (string), `added` (integer) and `removed` (integer), from `git diff --numstat` for the task's own change; a binary file reports `0` and `0`. | `200`, `401`, `404` |
| `/v1/repos/<name>/items/<task>/diff/<path>` | `GET` | `path` (string), `truncated` (boolean), `text` (string): that file's unified diff, cut at 256 KiB (262,144 bytes) with `truncated` `true`. A path not in the list is `404`. | `200`, `401`, `404` |
| `/v1/events` | `GET` | The event stream. Until it is built this route is `404`. | `404`, `401` |
| `/v1/repos/<name>/items/<id>/<action>` | `POST` | An owner action; see [Owner actions](#owner-actions). | `200`, `400`, `401`, `404`, `405`, `409`, `422`, `500` |

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

A successful action answers `200` with `{"ok": true, "text": "<the CLI's stdout text>"}`.
A failed action answers `{"ok": false, "text": "<the CLI's message>"}` and one of these status codes:

| Status | When |
|---|---|
| `400` | `reject` without a note: the CLI's empty-note message, and nothing changes. |
| `404` | The repository is not registered, or the id names no plan or task. |
| `405` | A write route was called with a method other than `POST`. |
| `409` | Another process holds `state.lock`, or the state does not allow the action (for example a merge on a task that is not `review.awaiting_human`). |
| `422` | A usage or validation error: a value outside the action's closed set, or another check the CLI refuses with exit `2` or with exit `3` for a value outside a set. |
| `500` | Any other error, such as a git or file failure. |

The route checks `state.lock` before it acts, so a held lock answers `409` with `state.lock is locked by another process` and changes no file.
It waits no longer than the CLI's own lock wait.

There is no second confirmation in the API: `merge`, `reject`, `auto` at level `merge` and `cancel` act at once.
**The client asks before it calls them.**

`start` and `reset-budget` are not routes.
Any other action name is `404` with `{"error": "no such route"}`, and `GET` on a write route is `405`.

## The loop

The scheduling loop is not built in this build: `radian hub` prints `hub: the scheduling loop is not built yet` and serves the HTTP API only.

## The `[hub]` configuration

`[hub]` lives in the user configuration file (`~/.radian/config/config.toml`); see [`configuration`](../configuration.md#hub).
A change takes effect at the hub's next start.

| Key | Type | Default | Allowed | What it does |
|---|---|---|---|---|
| `hub.port` | integer | `7341` | `1` to `65535` | The HTTP API's port. The API binds `127.0.0.1` only. |
| `hub.concurrency` | integer | `4` | `1` or more | Tasks in `doing` across every registered repository. The scheduling loop, which is not built in this build, uses it. |
| `hub.interval_secs` | integer | `30` | `5` to `3600` | Seconds between two scheduling passes. The scheduling loop, which is not built in this build, uses it. |

A value outside its range is a configuration error naming the key and the range.
An unknown key under `[hub]` is skipped and named at startup.
