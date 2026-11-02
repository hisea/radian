---
description: The hub: its HTTP API on a configurable address (loopback by default), pairing and the bearer token, every route, the repo sessions it reads, the repository registry, the scheduling loop, and the `[hub]` configuration keys.
---

# hub

A hub is one process that manages every repository registered on this machine.
[`radian hub`](#radian-hub) serves the HTTP API on `<hub.bind>:<hub.port>` (loopback by default) and runs the scheduling loop for every registered repository; `radian hub add`, `remove` and `list` manage the registry; [`radian hub pair`](#radian-hub-pair) and [`radian hub token rotate`](#radian-hub-token-rotate) handle the bearer token every request carries.
The API also reads the radian sessions on this machine: [the session routes](#sessions) list a repository's sessions, read a session's transcript, follow its event stream, list its models and read a worker's conversation.

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
It binds `<hub.bind>:<hub.port>`; `hub.bind` defaults to `127.0.0.1`, and there is no key that turns the listener off.
The bind happens after it takes `$RADIAN_HOME/hub/hub.lock`, a lock that holds the process's pid and lives as long as the process: a second `radian hub` exits `1` with a line that names `hub.lock` and the holder's pid, before it binds anything or touches any repository.
A port another program holds exits `1` with a line that names the port.

Its first stdout line is `hub: listening on <addr>:<port>`; an IPv6 address is in brackets, as in `hub: listening on [::1]:7341`.
A bind that is not loopback prints one warning line on stderr at startup: the bearer token and every request travel unencrypted on that network, and `tailscale serve` or an ssh forward is the encrypted way.
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

- `api_version` names the route set and its behavior. This build's is `2`, and every route starts with `/v1/`.
- `schema_version` names the plan and task file shape (see [the file fields](files.md)). This build's is `1`.

They change on their own: a route can be added without touching a plan or task file, and a field can be added to a file while `api_version` stays `2`.
Version `2` adds [the session routes](#sessions); every version `1` route and its behavior is unchanged, so a client written for `1` keeps working.
`GET /v1/version` returns both.

## Routes

The read routes and the event stream take `GET`; the owner actions take `POST` (see [Owner actions](#owner-actions)).
Every body is `application/json` except [the event stream](#the-event-stream), which is `text/event-stream`; a `204` has no body.
The list and show bodies are exactly the bytes `radian arc list --json` and `radian arc show --json` print in that repository, so a field the CLI gains appears on the route too.
Every task object in `/v1/repos/<name>/items` and in the plan and task bodies of `/v1/repos/<name>/items/<id>` carries `board_column`: the arc TUI board's column for that task at the moment the response was written — `todo`, `doing`, `verifying` or `needs_you`, and `null` for a finished task (`done.*` or `failed.*`) or when its plan cannot be read (see [the `list --json` fields](planning.md#radian-arc-list-plan-id-all-json)).

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
| `/v1/repos/<name>/sessions` | `GET` | `sessions` (array), the repository's sessions, live and ended, newest activity first; see [Sessions](#sessions). | `200`, `401`, `404`, `500` |
| `/v1/repos/<name>/sessions/<id>/transcript` | `GET` | `session` (string) and `entries` (array): the journal folded into [transcript entries](#transcript-entries). | `200`, `401`, `404`, `500` |
| `/v1/repos/<name>/sessions/<id>/events` | `GET` | A server-sent-event stream of the session's changes; see [the session event stream](#the-session-event-stream). | `200`, `401`, `404`, `409`, `500` |
| `/v1/repos/<name>/sessions/<id>/models` | `GET` | `models` (array): the control port's `models` answer, grouped by provider with the current route marked. | `200`, `401`, `404`, `409`, `500`, `502` |
| `/v1/repos/<name>/sessions` | `POST` | Starts a hub-held session and sends its first message: `session` (string), the new id. See [starting and driving a session](#starting-and-driving-a-session). | `200`, `400`, `401`, `404`, `413`, `422`, `500`, `502` |
| `/v1/repos/<name>/sessions/<id>/prompt` | `POST` | One message: `session` (string) and `message_id` (string). See [starting and driving a session](#starting-and-driving-a-session). | `200`, `400`, `401`, `404`, `413`, `422`, `500`, `502` |
| `/v1/repos/<name>/sessions/<id>/cancel` | `POST` | The control port's `cancel`: `ok` (boolean, `true`). | `200`, `401`, `404`, `409`, `500`, `502` |
| `/v1/repos/<name>/sessions/<id>/answer` | `POST` | The control port's `answer`, with the hub token: `ok` (boolean). A late answer is `409`. | `200`, `400`, `401`, `404`, `409`, `500`, `502` |
| `/v1/repos/<name>/sessions/<id>/config` | `POST` | The control port's `config`, applied at the next turn boundary: `ok` (boolean). | `200`, `400`, `401`, `404`, `409`, `422`, `500`, `502` |
| `/v1/repos/<name>/items/<task>/conversation` | `GET` | `task` (string), `attempt` (integer or `null`) and `entries` (array): one attempt's `result.jsonl` folded with the ACP fold. `?attempt=<n>` picks an attempt by number. | `200`, `400`, `401`, `404`, `500` |
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

## Sessions

The hub is also the remote door to the radian sessions on this machine: the sessions a `radian` or `radian exec` process hosts, and the ones that have ended.

**Discovery.** The hub reads `$RADIAN_HOME/run/*.sock` every 2 seconds and calls the control port's `list_sessions` on each socket whose file name is a live pid's.
It assigns each live session to a registered repository whose `path` is the same directory as the session's `workspace`, compared after canonicalizing both sides, so macOS's `/var` → `/private/var`, a symlinked project directory and an older spelling of the same directory are one workspace.
A live session in a workspace that is not registered is not shown anywhere.
A socket that does not answer within one second, or whose pid is gone, is not a session, and a session whose socket goes away leaves the list at the next scan (or the one after that, when its process is alive but its port stopped answering).

The ended sessions come from the same lookup `radian exec --list` uses: the session index under `$RADIAN_SESSION_DIR` (default `$RADIAN_HOME/sessions`), filtered by the repository's workspace.

The hub keeps one long-lived `subscribe` connection per live session, carrying the hub token, so the port treats it as the owner's. The connection reconnects when it drops; that is what feeds [the session event stream](#the-session-event-stream).
All of this runs on threads of the hub's own, never on the HTTP server's threads: a session's socket that accepts and never answers delays no request.

### `GET /v1/repos/<name>/sessions`

The repository's sessions, live and ended, newest activity first.
The body is one object with a `sessions` array; each entry has these fields:

| Field | Type | What it is |
|---|---|---|
| `id` | string | The session id. |
| `created` | string or `null` | The session's creation time, RFC 3339 UTC (the `SessionCreated` line's `ts`), or `null` when the index has no row for it. |
| `last_activity` | string or `null` | The time of the newest journal line, the same value `radian exec --list` prints, or `null`. |
| `title` | string or `null` | The first user message as one line, cut at 80 characters with `…`; `null` when the session has no user message or the index has no row for it. |
| `live` | string | `tui` when a `radian` or `radian exec` process hosts the session, `hub` for a session a hub-held process hosts (a process the hub started, or any process with the hub-held mode), `none` when it has ended. |
| `state` | string | The turn state: `idle`, `working` or `blocked` (the same classification the herdr report uses, with `blocked` winning over `working`). A live session's value is the one its port reports; an ended session reads `idle`. |
| `pending` | integer | The length of the approval and question queue. An ended session reads `0`. |
| `model` | object or `null` | The model route in force, `{"provider": string, "model": string}`, as the live session's port reports it; `null` for an ended session, whose route no process reports. |

A live session whose journal the index has not caught up with is still listed, with `created`, `last_activity` and `title` `null`.
An unknown repository is `404`; a session directory or its index that cannot be read is `500`.

### `GET /v1/repos/<name>/sessions/<id>/transcript`

The session's journal folded into transcript entries: the same entries `radian attach` draws, from the same fold.
The body is `{"session": "<id>", "entries": [ … ]}`.
The session must belong to that repository's workspace; another workspace's session, an unknown id, and an id that cannot be one are `404` with the same message `--export` and `--resume` give.

#### Transcript entries

Every entry is one object. The fields are always present, in this order; `kind` is the tag:

| Field | Type | What it is |
|---|---|---|
| `kind` | string | `user`, `assistant`, `reasoning`, `tool_call`, `tool_result`, `notice`, `compaction`, `outcome`, `bash`, `warning`, `tip`, `banner`, `startup`, `trust_hint` or `context`. The hub's folds produce `user`, `assistant`, `reasoning`, `tool_call`, `tool_result`, `notice`, `compaction`, `outcome` and `bash`; `bash` carries one extra field, `exclude_from_context` (boolean), and `outcome` carries `ok` (boolean). |
| `text` | string | The entry's display text: a message's text, a tool card's title and body line, a notice's line, a compaction's summary. An attachment is its placeholder line (`[file: P, N lines]`), never the file's body. |
| `is_error` | boolean | The entry is an error: a failed tool call, an interrupted reply. |
| `result` | boolean or `null` | A tool call's outcome once its result arrived; `null` while it runs. |
| `error_code` | string or `null` | A tool result's error code. |
| `card` | string | Where a tool entry sits in a merged card: `none`, `head`, `tail` or `solo`. |
| `tool` | object or `null` | A tool card's data; `null` for every other entry. |
| `compaction` | object or `null` | A compaction summary's data; `null` for every other entry. |
| `interrupted` | boolean | A committed assistant message was an interrupted prefix. |
| `images` | array | The images the entry's message carried, each with its display data but **never** the base64 payload; the placeholder line is in `text`. |
| `via` | string or `null` | `hub` on a user message the owner token sent through the control port; `null` on every other entry. |

### `GET /v1/repos/<name>/sessions/<id>/models`

`{"models": [ … ]}`: exactly the list `/model` shows in that session, grouped by provider, with the current route's entry marked `current: true`.
The hub asks the session's control port, so the list is the process's own (only logged-in providers, and the current route's provider).
A session no process hosts is `409`; a port that fails mid-request is `502`; an unknown session or repository is `404`.

### Starting and driving a session

The write routes start a session and carry the phone's messages, approvals and model changes into it. `POST /v1/repos/<name>/sessions` starts a **hub-held process** and sends its first message; `POST …/sessions/<id>/prompt` sends a later one, resuming an ended session first; `…/cancel`, `…/answer` and `…/config` forward the control port's own verbs.
A hub-held process is `radian exec` in its internal hub-held mode: it opens the session (new, or `--resume`), runs no turn by itself, takes every input from its control port, and sends approvals and questions to the session's pending queue, answered through `answer`, never a terminal.
It always binds its control port, whatever [`control.enabled`](../configuration.md#control) says: that port is its only input.
It is an ordinary session: its workspace is the repository's path, its configuration is the one the hub itself was started with (an explicit `--config` is passed on to the process), and its model is the request's `model` or the configuration's route.

| Route | Body |
|---|---|
| `POST /v1/repos/<name>/sessions` | `text` (string, required), `model` (object, optional), `images` (array, optional), `files` (array, optional). The first message is the same body's `text`, `images` and `files`. |
| `POST /v1/repos/<name>/sessions/<id>/prompt` | `text` (string, required), `steer` (boolean, default `false`), `images` (array, optional), `files` (array, optional). |
| `POST /v1/repos/<name>/sessions/<id>/cancel` | No field; an empty body is the same as `{}`. |
| `POST /v1/repos/<name>/sessions/<id>/answer` | `request_id` (integer, required) and `response`, the control port's own answer value. |
| `POST /v1/repos/<name>/sessions/<id>/config` | `config` (object, required), the control port's own change: `model` (`{provider, model}`) and/or `thinking`. |

`model` is exactly `{"provider": string, "model": string}`; any other key, a missing or empty value, or a value that is not an object is `400`.
`images` and `files` are the control port's own attachment shapes and limits ([attachments](../control.md#attachments)): a bad image, an over-limit file or an unknown MIME is `422` naming the item. A present `null` is a wrong-typed field, not an absent one.
A missing or wrong-typed `text`, `steer`, `images`, `files`, `request_id`, `response` or `config` is `400`.
A body is validated before any process starts, so a refused body starts nothing and changes nothing.
A session write route reads at most 16 MiB (16,777,216 bytes) minus 1 KiB of body and answers `413` beyond it: the control port's own request line is 16 MiB, and forwarding adds a small envelope.

A session a process already hosts — a TUI's or the hub's own — is never started twice: the prompt goes through that process's socket.
An ended session is resumed by a new hub-held process on the same journal.
An unknown session, or one whose workspace is another repository's, is `404`; `cancel`, `answer` and `config` on a session no process hosts are `409`; an answer that arrives after the queue entry was completed or abandoned is `409` (`the request is no longer pending: a late answer changes nothing`) and changes nothing; an unknown `request_id` is `404`; a port that fails mid-request is `502`.
A socket that has gone while the hub's last scan still listed the session is the ended-session case, not a port failure: a live-only route answers `409` (the same body as for an ended session) and a `prompt` resumes the journal, and the stale entry is dropped.

A hub-held process with no running turn, no pending approval or question and no running background job for longer than [`hub.session_idle_secs`](#the-hub-configuration) exits cleanly: the journal stays and the next prompt resumes it.
Only hub-started processes are reclaimed; a TUI decides its own session's life.
A hub that stops stops the hub-held processes it still has cleanly (SIGTERM, then a grace period).

### `GET /v1/repos/<name>/items/<task>/conversation`

An arc worker attempt's conversation: its `result.jsonl` folded with the ACP fold, the entries the arc TUI's conversation page shows.
The body is `{"task": "<task id>", "attempt": <integer> or null, "entries": [ … ]}`, in the [transcript entry](#transcript-entries) shape.

| Parameter | Where | Type | What it does |
|---|---|---|---|
| `attempt` | query string | integer | Reads that attempt (`<task>-<n>`) instead of the newest one. |

Without `attempt` the newest attempt is read.
A task with no attempt answers `200` with `"attempt": null` and no entries.
An unknown task, or an attempt number the task does not have, is `404`; an `attempt` value that is not a number is `400`. The task must be a task, not a plan.

### The session event stream

`GET /v1/repos/<name>/sessions/<id>/events` is a server-sent-event stream of one session's changes, fed by the hub's own subscription.
It needs the bearer token and answers `Content-Type: text/event-stream`.
A session no process hosts is `409`; an unknown session or repository is `404`.

The stream follows the [event stream](#the-event-stream)'s rules: no event ids, no replay, a `: keep-alive` comment every 15 seconds while nothing happens, and an end when the client disconnects, the session's process goes away or the hub stops.
**A client that reconnects first re-reads the transcript.**

Each event has one of these names, with this `data` object:

| Event | `data` | When |
|---|---|---|
| `entry` | `index` (integer), `reset` (boolean), `entry` (one [transcript entry](#transcript-entries)) | An entry was added or changed at `index`. `reset` is `true` on the first frame after the fold was rebuilt, and the client then drops every entry before `index`. |
| `pending` | `head` (object or `null`) and `length` (integer) | The approval and question queue's head changed. The head carries the request id, the kind (`approval` or `question`) and, for an approval, the tool, the resolved resource and the raw arguments. |
| `config` | `revision` (integer) and `change` (object) | A `ConfigChanged` record was committed: a `/model`, `/thinking`, or hub `config` change applied at a turn boundary. |
| `state` | `state` (string) | The turn state changed: `idle`, `working` or `blocked`, from the queue's length and the journal's open turn. The process's own facts (a trust modal, background jobs) are on the `sessions` route, which asks the port. |

## Notifications

A lock-screen notification for a task is a [`[hooks]`](hooks.md#hooks) command, not something the hub sends.
These arc points run in the scheduling loop, so the notification is sent whether or not a client is connected.
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

### The after_pending hook

`[hub.hooks] after_pending` in the user configuration (`~/.radian/config/config.toml`) is an array of shell command lines, default `[]`, in the same shape as arc's [`[hooks]`](hooks.md#hooks) points: each entry is one `sh -c` line.
The hub runs the list once each time a live session's approval and question queue gets a **new head**, that is, a head whose `request_id` the hub has not seen for that session. It does not run on a repeat of the same head (a subscription reconnect re-sends it), and it does not run when the queue empties. There is no hub process, there is no notification.

The commands run in the hub's process, from the same long-lived `subscribe` connection that feeds [the session event stream](#the-session-event-stream). Each fire runs on a thread of its own, so a slow or failing command never delays the subscription, another session's hook or an HTTP request. A command that fails or times out does not stop the next one.
The hook list is read when the hub starts; a change takes effect at the hub's next start.

| Variable | Value |
|---|---|
| `RADIAN_HUB_REPO` | The repository's registry name. |
| `RADIAN_HUB_SESSION` | The session id. |
| `RADIAN_HUB_PENDING_KIND` | `approval` or `question`. |
| `RADIAN_HUB_TOOL` | The approval's tool name; empty for a question. |

Those four values are the whole of what the request contributes, the privacy rule that governs notification reports: no command text, path, tool arguments or question text reaches the command. The client opens the session over the hub for the rest.
A live session in a workspace that is not registered runs nothing, because it has no repository name.

Each command runs in the repository root with that repository's arc [`gate.command_timeout_secs`](config.md#configtoml) as its limit (default `3600` seconds; `0` means no limit), the same rule arc's own hooks follow. A command past its limit is killed with its process group.
A repository whose `.radian/arc/config.toml` cannot be read runs none of the commands; one line naming the reason goes to `hub.log`.
Its stdout, stderr, and one line per failure or timeout are appended to `$RADIAN_HOME/hub/hub.log` (default `~/.radian/hub/hub.log`), the directory the sandbox cannot read.

One ntfy push, using only those variables:

```toml
[hub.hooks]
after_pending = [
  "curl -s -d \"$RADIAN_HUB_REPO $RADIAN_HUB_SESSION $RADIAN_HUB_PENDING_KIND $RADIAN_HUB_TOOL\" https://ntfy.sh/<your-topic>",
]
```

This is a different point from arc's [`after_awaiting_human`](hooks.md#hooks): that one runs in the scheduling loop for a task, and this one runs in the hub for a radian session. Neither depends on the other.

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
| `hub.bind` | string | `127.0.0.1` | an IPv4 or IPv6 address, not a host name or an address with a port | The IP address the HTTP API listens on. A bind that is not loopback prints one warning at startup: the bearer token and every request travel unencrypted on that network; `tailscale serve` or an ssh forward is the encrypted way. |
| `hub.port` | integer | `7341` | `1` to `65535` | The HTTP API's port. |
| `hub.concurrency` | integer | `4` | `1` or more | The machine-wide cap on tasks in `doing` across every registered repository. |
| `hub.interval_secs` | integer | `30` | `5` to `3600` | Seconds between two registry reads and two scheduling passes of each repository. |
| `hub.session_idle_secs` | integer | `3600` | `60` to `86400` | How long a hub-held session may have no running turn, nothing pending and no running background job before its process exits cleanly; the journal stays and the next prompt resumes it. |
| `hub.hooks.after_pending` | array of strings | `[]` | shell command lines | The commands the hub runs when a live session's queue gets a new head; see [the after_pending hook](#the-after-pending-hook). |

A value outside its range is a configuration error naming the key and the range.
A `hub.bind` that is not an IP address (a host name, an empty string, an address with a port) is a configuration error naming the key and "an IP address".
An unknown key under `[hub]` is skipped and named at startup.
