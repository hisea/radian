---
description: The local process control port: its socket, the owner token, every verb and the event stream.
---

# The process control port

Every running radian process that hosts a session — the full-screen interface (`radian`) and
`radian exec` — opens one Unix socket and serves a JSON-lines protocol on it. A local script,
`radian attach`, or a hub uses it to send input, cancel a turn, read the session, follow its
committed events, and (with the owner token) answer the pending approval or question and change
the model route. It is a local IPC surface, never a remote protocol.

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
client's word alone:

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
that opens it and every `steer`/`inject` that turn admits. A queued scheduled input can
lower the turn an owner's `followup` starts, and it does so before the first model call.

## The owner token

A request may carry an optional string field `token`. When it equals the trimmed content of
`$RADIAN_HOME/hub/token` (read on every request, so a rotation takes effect at the next
request), the request is the owner's:

- `followup`, `steer` and `inject` enqueue with source `User`; the `source` field is ignored.
- The message records `{"via": "hub"}` in its `origin` metadata, so the transcript can tell a
  phone-sent message from a typed one.
- The four owner verbs below are available.

A missing, unreadable or empty token file means no token matches. A request without `token`,
or with a wrong one, behaves exactly as without this section: source `Scheduler` by default, no
`via` metadata, the `source` rules above, and the four owner verbs return an error line and
change nothing. The comparison is constant-time.

## The verbs

| verb | fields | result |
|---|---|---|
| `followup` | `text`, `source?`, `token?`, `images?`, `files?` | enqueues a new turn; `message_id` |
| `steer` | `text`, `source?`, `token?`, `images?`, `files?` | redirects the running turn |
| `inject` | `text`, `source?`, `token?`, `images?`, `files?` | adds context without waking an idle session |
| `cancel` | — | cancels the running turn (idempotent) |
| `list_sessions` | — | `sessions`: one `main` row (below) |
| `subscribe` | `token?` | `{"ok": true}`, then event lines (below) |
| `pending` | `token` | `head` (or `null`) and `length` |
| `answer` | `request_id`, `response`, `token` | completes one pending entry |
| `config` | `config`, `token` | queues one turn-boundary change |
| `models` | `token` | the `/model` list, grouped by provider |

The task verbs (`spawn_task`, `wait_task`, `task_status`) do not exist. An unknown verb
(including `"approve"`) returns an error and changes nothing.

## `pending`

```json
{"id": 7, "verb": "pending", "token": "…"}
```

```json
{"id": 7, "ok": true, "head": {"request_id": 3, "kind": "approval", …}, "length": 1}
```

`head` is `null` and `length` is `0` on an empty queue. `length` is the queue's valid pending
count. `request_id` is the integer queue id that `answer` takes; it grows with every push.

An approval head:

```json
{"request_id": 3,
 "kind": "approval",
 "call_id": "call_abc",
 "tool": "write",
 "resource": "out.txt",
 "resolved_resource": "/home/me/project/out.txt",
 "arguments": {"path": "out.txt", "content": "hi"},
 "reason": null,
 "approval_id": "p-17"}
```

`resource` is the resource as the call wrote it; `resolved_resource` is the path or command the
permission rules actually matched (a prompt shows this one); `arguments` is the call's raw JSON;
`reason` is the rule's reason, or `null`; `approval_id` is the permission layer's own request
id.

A question head carries the full structured request:

```json
{"request_id": 4, "kind": "question",
 "question": {"questions": [
   {"id": "loader", "prompt": "How should it load?", "kind": "single",
    "options": [{"id": "builtin", "label": "Built-in", "recommended": true}],
    "allow_other": true, "required": true}]}}
```

## `answer`

```json
{"id": 8, "verb": "answer", "request_id": 3, "response": "allow_once", "token": "…"}
```

An approval response is one of:

| `response` | effect |
|---|---|
| `"allow_once"` | allow this call once |
| `"allow_always"` | `AllowForSession`: a process-local grant for this call's tool and resource, the same as the ACP surface's `allow_always` |
| `"reject_once"` | deny; the tool result says “denied by the user” |
| `{"kind": "reject_once", "reason": "…"}` | deny with that reason; it lands verbatim in the tool result |

The same three may be written as objects: `{"kind": "allow_once"}` or
`{"kind": "allow_always"}` (no `reason`), and the `reject_once` object above.

A question response carries one answer per question:

```json
{"response": {"answers": [
  {"id": "loader", "selected": ["builtin"]},
  {"id": "name", "text": "radian"},
  {"id": "tiers", "selected": ["safe", "workspace"], "other": "a custom tier",
   "note": "…"}]}}
```

Each answer accepts `id`, `selected` (option ids), `text` (free text), `other` (the `Other`
text) and `note` (at most 500 characters). The answer's `request_id` is optional; when absent
the port uses the TUI's `q-<queue id>`, so a port answer and a keyboard answer land the same
tool result. Free text, multi-select and `Other` are all answerable.

The pending queue is the arbiter: the first answer wins. A successful `answer` completed that
entry. An error line changes nothing when:

- the id is unknown: `no pending request has that id`;
- the entry was already answered (by the TUI, ACP, or an earlier port answer) or abandoned:
  `the request is no longer pending: a late answer changes nothing`;
- the response is for the other kind (an approval response for a question, or the reverse);
- the response does not parse: an unknown approval word, a non-string `reason`, or an `answers`
  list that violates the answer shape.

## `config`

```json
{"id": 9, "verb": "config", "token": "…",
 "config": {"model": {"provider": "anthropic", "model": "claude-sonnet-4"}}}
{"id": 10, "verb": "config", "token": "…", "config": {"thinking": "high"}}
```

The change goes to the turn-boundary configuration queue and is applied before the next turn,
recording one `ConfigChanged` in the journal — the same path as `/model` and `/thinking`. An
in-flight model request keeps the old route; the next turn uses the new one.

| key | type | accepted values |
|---|---|---|
| `model` | object | exactly `{"provider": string, "model": string}`. The route must be the current route or one entry of `models`; any other provider/model is refused. |
| `thinking` | string | `off`, `minimal`, `low`, `medium`, `high`, `xhigh` or `max`. Any other value is refused. |

Both keys may appear in one object; they are applied as one change. An empty object, an unknown
key, a non-object `model`, a missing or empty `provider` or `model`, or an unknown route
returns an error line and queues nothing. The key and header values are never part of the
change.

## `models`

```json
{"id": 11, "verb": "models", "token": "…"}
```

```json
{"id": 11, "ok": true, "models": [
  {"provider": "anthropic", "models": [
    {"id": "claude-sonnet-4", "current": true},
    {"id": "claude-opus-4", "current": false}]},
  {"provider": "opencode-go", "models": [
    {"id": "deepseek-v4.1-flash", "current": false}]}]}
```

The same list `/model` shows for this session, grouped by provider and with the current route's
entry marked `current: true`. Groups follow `/model`'s order: the current provider first, then
providers and models by name. Only logged-in providers (a usable `auth.json` entry) and the
current route's provider are considered; a provider with no known model contributes no group.
A `config` `model` must name one of these entries or the current route.

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
| `state` | `idle`, `working` or `blocked`; a non-empty pending queue is `blocked`, a running turn (or background jobs) is `working`, and everything else is `idle` |
| `pending` | the pending approval/question count |

None of these fields needs the owner token.

## `subscribe` and events

After the `subscribe` response, the server writes committed events as lines:

```json
{"event": {"source": "agent", "name": "tool_started", "data": {"index": 0, "call_id": "…", "name": "read"}}}
{"event": {"source": "runtime", "name": "TurnFinished", "data": {"turn_id": "…", "outcome": "completed", …}}}
```

The agent events are the committed `AgentEvent`s the interface receives, minus the live-only
streaming deltas (`model_text_delta`, `model_reasoning_delta`). The runtime events are the
driver's (`TurnFailed`, `TurnFinished`, `ConfigChanged`, …). A queue change adds one `pending`
event:

```json
{"event": {"source": "control", "name": "pending",
           "data": {"head": {"request_id": 3, "kind": "approval", "tool": "write", …},
                     "length": 1}}}
```

It is written whenever the pending queue's version changes (a push, an answer, an abort, or an
abandoned wait disappearing). `head` is `null` and `length` is `0` on an empty queue. A
subscriber that sent `token` with `subscribe` gets the same head `pending` returns. A subscriber
without the token gets a redacted head with only `request_id`, `kind` (`"approval"` or
`"question"`) and `tool` (the tool name for an approval, `null` for a question): no arguments,
no resource, no question text. No token deltas are pushed; the event stream stays committed
facts only.

One client may issue requests and subscribe on the same connection. The per-client buffer is
bounded: a subscriber that falls behind is disconnected, and the session never waits on a
client.

## Attachments

`followup`, `steer` and `inject` also accept optional attachments, with or without a token:

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
sock="$RADIAN_HOME/run/$$.sock"  # the hosting process's pid, not the shell's
printf '%s\n' '{"id":1,"verb":"subscribe"}' | nc -U "$sock"
printf '%s\n' '{"id":2,"verb":"followup","text":"run the tests"}' | nc -U "$sock"
printf '%s\n' '{"id":3,"verb":"pending","token":"…"}' | nc -U "$sock"
```
