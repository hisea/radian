---
description: The local process control port: its socket, every verb and the event stream.
---

# The process control port

Every running radian process that hosts a session — the full-screen interface (`radian`) and
`radian exec` — opens one Unix socket and serves a JSON-lines protocol on it. A local script
or `radian attach` uses it to send input, cancel a turn, read the session and follow its
committed events. It is a local IPC surface, never a remote protocol. Approvals and questions
are not answered through it: the session that asked answers them (the TUI popup, or
`radian exec`'s stdin prompt).

## Where the socket is

- `$RADIAN_HOME/run/<pid>.sock`, where `$RADIAN_HOME` is `$RADIAN_HOME` when set, else
  `$HOME/.radian`.
- The `run/` directory is `0700` and the socket file is `0600`.
- The file is removed when the process exits, including a panic unwind. At startup a `*.sock`
  whose pid is no longer alive is removed, so a `kill -9` does not leave one behind.
- The port is on by default. `[control] enabled = false` in the config file turns it off for
  that process (see [`configuration`](configuration.md#control)); the startup line then prints
  no `[control]` path. When it is on, startup prints
  `[control] /home/me/.radian/run/12345.sock`.

## The protocol

One JSON object per line, in both directions. A request:

```json
{"id": 1, "verb": "followup", "text": "hello"}
```

A response:

```json
{"id": 1, "ok": true}
```

or

```json
{"id": 1, "ok": false, "error": "…"}
```

`id` is echoed back and is whatever JSON value the client sent; a request without a parseable
`id` gets `"id": null`. The verb's own result fields sit at the top level of a successful
response. A successful response to a verb with a result is the `id`, `ok`, and the result's
fields.

One request is one JSON line of at most 16 MiB (16 777 216 bytes), excluding the newline. A
longer line is discarded through its newline and answered with
`the request line is over the 16 MiB limit`; the connection stays usable.

## Sources

`followup`, `steer` and `inject` carry an input whose source the port decides, never the
client's word alone. There is no owner: every input that arrives through the port is
non-human.

| `source` | result |
|---|---|
| absent, `"scheduler"` | `Scheduler` (the default) |
| `"subagent"` | `Subagent` |
| `"mcp"` | `Mcp` |
| `"user"` | refused — the port never accepts a self-declared `User` |
| `"permission_decision"` | refused |
| anything else | refused |

A turn that is already running keeps its own effective source, which only ever lowers: a
`steer` or `inject` that joins it can lower it, never raise it. A `followup` that arrives
mid-turn belongs to a later turn and leaves the running turn alone.

The same rule applies at a turn's start: its source is the least-privileged of the input
that opens it and every `steer`/`inject` that turn admits.

## The verbs

| verb | fields | result |
|---|---|---|
| `followup` | `text`, `source?`, `images?`, `files?` | enqueues a new turn; `message_id` |
| `steer` | `text`, `source?`, `images?`, `files?` | redirects the running turn |
| `inject` | `text`, `source?`, `images?`, `files?` | adds context without waking an idle session |
| `cancel` | — | cancels the running turn (idempotent) |
| `list_sessions` | — | `sessions`: one `main` row (below) |
| `subscribe` | — | `{"ok": true}`, then event lines (below) |

The task verbs (`spawn_task`, `wait_task`, `task_status`) do not exist. Any other verb
(including `pending`, `answer`, `config`, `models`, `close` and `approve`) returns an error and
changes nothing.

## `list_sessions`

```json
{"id": 1, "verb": "list_sessions"}
```

```json
{"id": 1, "ok": true, "sessions": [{
  "id": "...", "kind": "main",
  "workspace": "/home/me/project",
  "model": {"provider": "anthropic", "model": "claude-sonnet-4"},
  "state": "blocked",
  "pending": 1}]}
```

One process serves one session, so the list has one `main` row:

| field | meaning |
|---|---|
| `id` | the session id |
| `kind` | always `main` |
| `workspace` | the canonical workspace the session was opened in |
| `model` | the model route in force |
| `state` | `idle`, `working` or `blocked`; a non-empty pending queue is `blocked`, a running turn (or background jobs) is `working`, an idle block (a used-up wake budget, a `/goal` budget stop, a lost job notice, the intent-stop cap or the idle hook-continuation cap) is `blocked` while no turn runs, and everything else is `idle` |
| `pending` | the count of approvals and questions the session is waiting on |

## `subscribe` and events

After the `subscribe` response, the server writes committed events as lines:

```json
{"event": {"source": "agent", "name": "tool_started", "data": {"index": 0, "call_id": "…", "name": "read"}}}
{"event": {"source": "runtime", "name": "TurnFinished", "data": {"turn_id": "…", "outcome": "completed", …}}}
```

The agent events are the committed `AgentEvent`s the interface receives, minus the live-only
streaming deltas (`model_text_delta`, `model_reasoning_delta`). The runtime events are the
driver's (`TurnFailed`, `TurnFinished`, `ConfigChanged`, …). No token deltas are pushed; the
event stream stays committed facts only.

One client may issue requests and subscribe on the same connection. The per-client buffer is
bounded: a subscriber that falls behind is disconnected, and the session never waits on a
client.

## Attachments

`followup`, `steer` and `inject` also accept optional attachments:

| field | type | what it is |
|---|---|---|
| `images` | array of `{"mime": string, "data": string}` | `data` is standard base64. `mime` must be `image/png`, `image/jpeg`, `image/gif` or `image/webp`; the bytes' magic number decides the kind. The limits are the interface composer's: at most 5 MiB and 8000 px per side. |
| `files` | array of `{"name": string, "text": string}` | The text becomes the attached-file part named `name`, without reading anything from disk. At most 100 KiB per file and 400 KiB in total. |

They become the message's content parts through the same composer the interface uses for a
dropped image and an `@` file: the text part, then one image part per image, then one
attached-file part per file. An image's display name is `image <n>` (1-based). `text` is
required and may be empty when the request carries only attachments. A bad attachment — an
unknown MIME, invalid base64, an image or file over a limit — rejects the whole request with an
error naming the item, and nothing is enqueued.

## Example

```sh
# One socket per hosting process: $RADIAN_HOME/run/<pid>.sock. Pick the hosting process
# you want; `$$` is this shell's pid, not the hosting process's.
pid="$(ls "$RADIAN_HOME/run" | sed 's/\.sock$//' | head -n 1)"
sock="$RADIAN_HOME/run/$pid.sock"
printf '%s\n' '{"id":1,"verb":"subscribe"}' | nc -U "$sock"
printf '%s\n' '{"id":2,"verb":"followup","text":"run the tests"}' | nc -U "$sock"
```
