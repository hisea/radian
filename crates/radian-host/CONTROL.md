# The process control port (ADR 0043)

Every Radian process that hosts an interactive (`radian`) or line (`radian exec`) session opens
a Unix socket and serves a small JSON-lines protocol on it. It is the seam an external driver
(a project script, herdr, or `radian attach`) uses to drive or observe a session without a TUI:
enqueue inputs, cancel, list the hosted session, and follow the committed events.

There is **no** separate `radian serve` in this version: the process that owns a session owns
its socket. (`radian serve` and `radian mcp` remain reserved subcommands.)

## Where the socket is

- `$RADIAN_HOME/run/<pid>.sock`. `$RADIAN_HOME` is `$RADIAN_HOME` when set, else `$HOME/.radian`.
- The `run/` directory is created `0700`; the socket file is `0600`.
- The socket is removed when the process exits — the normal path and the panic-unwind path
  both drop a guard that removes it. At startup every `*.sock` in `run/` whose pid is no longer
  alive (`kill(pid, 0)`, `EPERM` counts as alive) is removed, so a `kill -9` does not leave one.
- On by default. `[control] enabled = false` in the config file turns the port off for this
  process; the startup line then does not print a `[control]` path.
- The startup writes one line naming the path: `[control] /home/me/.radian/run/12345.sock`.

## The protocol

One JSON object per line, both directions. A request is:

```json
{"id": 1, "verb": "followup", "text": "hello"}
```

A response is:

```json
{"id": 1, "ok": true}
```

or

```json
{"id": 1, "ok": false, "error": "..."}
```

The verb's own result fields are at the top level of a successful response. `id` is echoed
back and is whatever JSON value the client sent (a number is conventional); a request without a
parseable `id` gets `"id": null`.

### Sources

`followup`, `steer` and `inject` carry an input whose source the port decides, never the
client's word alone; that source is what the inbox records. A turn that is already running keeps
its own effective source, which only ever lowers: a request that arrives mid-turn never raises
it, not even the owner's. Without a matching owner token (below), the client picks the input's
source with an optional `source` field:

| `source` | result |
|---|---|
| absent, `"scheduler"` | `InputSource::Scheduler` (the default) |
| `"subagent"` | `InputSource::Subagent` |
| `"mcp"` | `InputSource::Mcp` |
| `"user"` | **refused** — the port never accepts a self-declared `User` (ADR 0043) |
| `"permission_decision"` | **refused** |
| anything else | refused |

### Owner token

A request may carry an optional string field `token`. When it equals the trimmed content of
`$RADIAN_HOME/hub/token` (read when the request arrives, so a rotation takes effect at the next
request), the request is the owner's:

- `followup`, `steer` and `inject` enqueue with source `User`; the `source` field is ignored.
- The message's `origin` metadata is `{"via": "hub"}` (ADR 0005 G1), so the transcript can tell
a phone-sent message from a typed one.

A missing, unreadable or empty token file means no token matches. A request without `token`, or
with a wrong one, behaves exactly as without this section: source `Scheduler` by default, no
`via` metadata, and the `source` rules above. The comparison is constant-time.

Approvals cannot be answered over the port: there is deliberately no approval or answer verb,
and an unknown verb (including `"approve"`) returns an error and changes nothing.

### Attachments

`followup`, `steer` and `inject` also accept optional attachments, with or without a token:

| field | type | what it is |
|---|---|---|
| `images` | array of `{ "mime": string, "data": string }` | `data` is standard base64. `mime` must be `image/png`, `image/jpeg`, `image/gif` or `image/webp`; the bytes' magic number decides the kind. The image limits are the TUI composer's: at most 5 MiB and 8000 px per side. |
| `files` | array of `{ "name": string, "text": string }` | The text is attached as the file part named `name`, without reading anything from disk. At most 100 KiB per file and 400 KiB in total. |

They become the message's content parts through the same composer the TUI uses for a dropped
image and an `@` file: the text part, then one image part per image, then one attached-file part
per file. An image's display name is `image <n>` (1-based). `text` is required, and may be empty
when the request carries only attachments. A bad attachment — an unknown MIME, invalid base64,
an image over a limit, or a file over a limit — rejects the whole request with an error line
naming the item, and nothing is enqueued.

### The request line limit

One request is one JSON line. A line may be at most 16 MiB (16 777 216 bytes) excluding its
terminating newline: enough for a few phone photos once base64 has grown them. A longer line is
discarded through its newline and answered with one error line
(`the request line is over the 16 MiB limit`); the connection stays usable, so the client can
retry with a smaller request.

### Verbs

| verb | fields | result |
|---|---|---|
| `followup` | `text`, `source?`, `token?`, `images?`, `files?` | enqueues a new turn; `message_id` |
| `steer` | `text`, `source?`, `token?`, `images?`, `files?` | redirects the running turn |
| `inject` | `text`, `source?`, `token?`, `images?`, `files?` | adds context without waking an idle session |
| `cancel` | — | cancels the running turn (idempotent) |
| `list_sessions` | — | `sessions`: the hosting `main` session (one process serves one session) |
| `subscribe` | — | response `{"id":n,"ok":true}`, then event lines (see below) |

The task verbs (`spawn_task`, `wait_task`, `task_status`) are not part of the port: there is no
in-process orchestration (ADR 0043 decision 1); the port now serves input, cancellation,
listing and event streaming only.

### `subscribe`

After the `subscribe` response, the server writes committed events as lines:

```json
{"event": {"source": "agent", "name": "tool_started", "data": {"index": 0, "call_id": "...", "name": "read"}}}
{"event": {"source": "runtime", "name": "TurnFinished", "data": {"turn_id": "...", "outcome": "completed", ...}}}
```

The agent events are the same committed `AgentEvent`s the TUI receives, minus the live-only
streaming deltas (`model_text_delta`, `model_reasoning_delta`). The runtime events are the
`RuntimeEvent`s the driver raises (`TurnFailed`, `TurnFinished`, `ConfigChanged`, …).

One client may issue requests and subscribe on the same connection. The per-client buffer is
**bounded**: a subscriber that falls behind is disconnected and its connection is closed; the
session never waits on a client.

## Example: send a turn and follow the events

```sh
sock="$RADIAN_HOME/run/$$.sock"  # the hosting process's pid, not the shell's
printf '%s\n' '{"id":1,"verb":"subscribe"}' | nc -U "$sock"
printf '%s\n' '{"id":2,"verb":"followup","text":"run the tests"}' | nc -U "$sock"
```
