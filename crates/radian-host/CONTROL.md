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
  process; the startup line then does not print a `[control]` path. A hub-held process (ADR
  0048 §16) is the exception: its port is its only input and the hub's only channel to it, so
  the mode binds the port whatever `[control] enabled` says, and a bind failure is a startup
  error for that process.
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
client's word alone; that source is what the inbox records, and the turn the input opens runs
with it. A turn that is already running keeps its own effective source, which only ever lowers:
a `steer` or `inject` that joins it can lower it, never raise it, not even the owner's. A
`followup` that arrives mid-turn belongs to a later turn: it leaves the running turn's source
alone and opens its own turn with its own source. The same rule holds at that turn's start: its
source is the least-privileged of the input that opens it and every `steer`/`inject` the turn
admits, so a queued scheduled input can lower the turn an owner's `followup` starts. Without a
matching owner token (below), the client picks the input's source with an optional `source`
field:

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

The same token gates the four owner verbs — `pending`, `answer`, `config` and `models` (below).
Without the matching token each returns an error line and changes nothing; `subscribe` stays
token-free but redacts the `pending` event (see below).

### Owner verbs

All four require `token` equal to `$RADIAN_HOME/hub/token` (trimmed). A missing, empty or wrong
token returns `{"ok": false, "error": "this verb needs the owner token: …"}` and changes
nothing.

#### `pending`

```json
{"id": 7, "verb": "pending", "token": "…"}
```

Result (`head` is `null` on an empty queue):

```json
{"id": 7, "ok": true, "head": {"request_id": 3, "kind": "approval", …}, "length": 1}
```

`request_id` is the queue id `answer` takes (an integer that grows per push). An approval head
also carries `call_id`, `tool` (the tool name), `resource` (the resource as the call wrote
it), `resolved_resource` (the path or command the rules matched — what a prompt shows),
`arguments` (the call's raw JSON arguments), `reason` (the rule's reason, or `null`) and
`approval_id` (the permission layer's own request id). A question head carries the full
`QuestionRequest` as `question: {"questions": […]}` (ADR 0024 Q2).

#### `answer`

```json
{"id": 8, "verb": "answer", "request_id": 3, "response": "allow_once", "token": "…"}
```

An approval response is one of:

| `response` | effect |
|---|---|
| `"allow_once"` | `ApprovalOutcome::AllowOnce` |
| `"allow_always"` | `ApprovalOutcome::AllowForSession`: a process-local grant, exactly the ACP surface's `allow_always` |
| `"reject_once"` | `ApprovalOutcome::Deny { reason: null }`; the permission layer turns this into “denied by the user” |
| `{"kind": "reject_once", "reason": "…"}` | `ApprovalOutcome::Deny { reason }`; the reason lands verbatim in the tool result |

The same three may be written as objects: `{"kind": "allow_once"}` or
`{"kind": "allow_always"}` (no `reason`), and the `reject_once` object above.

A question response carries the `QuestionAnswer` as `answers`, one object per question:

```json
{"response": {"answers": [{"id": "loader", "selected": ["builtin"]},
                           {"id": "name", "text": "radian"},
                           {"id": "tiers", "selected": ["safe"], "other": "a custom tier"}]}}
```

Each `QuestionReply` accepts `id`, `selected`, `text`, `other` and `note` (ADR 0024 Q3). The
answer's `request_id` is optional; when absent the port derives the TUI's `q-<queue id>`, so a
port answer and a keyboard answer land the same tool result. Free text, multi-select and
`Other` are all answerable (more than the ACP surface can represent, ADR 0024 Q9).

The queue is the arbiter. Success means this request completed the entry. `answer` returns an
error and changes nothing when:

- the id is unknown: `no pending request has that id`;
- the entry was already answered (by the TUI, ACP, or an earlier port answer) or abandoned:
  `the request is no longer pending: a late answer changes nothing`;
- the response is for the other kind (an approval response for a question, or the reverse);
- `response` does not parse (an unknown approval word, a non-string `reason`, an `answers` list
  that violates `QuestionReply`'s shape).

#### `config`

```json
{"id": 9, "verb": "config", "token": "…",
 "config": {"model": {"provider": "anthropic", "model": "claude-sonnet-4"}}}
{"id": 10, "verb": "config", "token": "…", "config": {"thinking": "high"}}
```

The change goes into the driver's `ConfigQueue` and is applied at the next turn boundary with
one `ConfigChanged` record — the `/model` and `/thinking` path (ADR 0004 path B, ADR 0026 D5).
An in-flight model request keeps the old route; the next turn uses the new one (ADR 0004 D4).
`config` accepts only these keys, and queues nothing when any check fails:

| key | type | accepted values |
|---|---|---|
| `model` | object | exactly `{"provider": string, "model": string}`. The route must be the route in force or one entry of `models` (below). Any other provider/model is an error. |
| `thinking` | string | one of `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max` (the `/thinking` set). Any other value is an error. |

Both keys may appear in one object, and are applied as one change. An empty object, an unknown
key, a non-object `model`, a missing or empty `provider`/`model`, or an unknown route returns an
error line and queues nothing.

#### `models`

```json
{"id": 11, "verb": "models", "token": "…"}
```

Result: the same list `/model` shows for this session (ADR 0026 D5), grouped by provider and
with the current route marked.

```json
{"id": 11, "ok": true, "models": [
  {"provider": "anthropic", "models": [
    {"id": "claude-sonnet-4", "current": false},
    {"id": "claude-opus-4", "current": true}]},
  {"provider": "opencode-go", "models": [
    {"id": "deepseek-v4.1-flash", "current": false}]}]}
```

The groups follow `/model`'s order: the current provider first, then providers and models by
name. The current route's entry has `current: true` when it appears in the list. Only logged-in
providers (a usable `auth.json` entry) and the current route's provider are considered; a
provider with no known model contributes no group. A `config` `model` must name one of these
entries or the current route.

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
| `list_sessions` | — | `sessions`: the hosting `main` session and its workspace, model route, state and pending count (below) |
| `subscribe` | `token?` | response `{"id":n,"ok":true}`, then event lines (see below) |
| `pending` | `token` | `head` (or `null`) and `length`; see *Owner verbs* |
| `answer` | `request_id`, `response`, `token` | completes one pending entry; see *Owner verbs* |
| `config` | `config`, `token` | queues one turn-boundary change; see *Owner verbs* |
| `models` | `token` | `models`, grouped by provider; see *Owner verbs* |

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

### `subscribe` and the `pending` event

The committed `AgentEvent` and `RuntimeEvent` lines above are joined by one `pending` event,
written whenever the pending queue's version changes (a push, an answer, an abort, or an
abandoned wait disappearing):

```json
{"event": {"source": "control", "name": "pending",
           "data": {"head": {"request_id": 3, "kind": "approval", "tool": "write", …},
                     "length": 1}}}
```

`head` is `null` and `length` is `0` when the queue is empty. A subscriber that sent `token`
on its `subscribe` request gets the same head `pending` returns. A subscriber without the
token gets the redacted head: `request_id`, `kind` (`"approval"` or `"question"`) and `tool`
(the tool name for an approval, `null` for a question) only — no arguments, no resource and no
question text. The event is still a committed fact: no token or reasoning deltas are pushed
(ADR 0043).

### `list_sessions`

One `main` row (one process serves one session):

```json
{"id": 1, "ok": true, "sessions": [{
  "id": "…", "kind": "main", "holder": "tui",
  "workspace": "/home/me/project",
  "journal": "/home/me/.radian/sessions/….jsonl",
  "model": {"provider": "anthropic", "model": "claude-sonnet-4"},
  "state": "blocked",
  "pending": 1}]}
```

| field | meaning |
|---|---|
| `workspace` | the canonical workspace this session was opened in |
| `journal` | the journal file this process has open, so two sessions with the same user-chosen id under different session directories are told apart |
| `holder` | `hub` for a process a hub started in its hub-held mode (ADR 0048 §16), `tui` for every other hosting process. The hub turns `hub` into `live: hub` and `tui` into `live: tui`. |
| `model` | the model route in force (ADR 0026 D8) |
| `state` | `idle`, `working` or `blocked` by ADR 0034 H2: a non-empty pending queue is `blocked`, a running turn (or background jobs) is `working`, an idle block (the wake budget, a `/goal` budget, a lost job notice, the intent-stop cap or the idle hook-continuation cap) is `blocked` while no turn runs, and everything else is `idle` |
| `pending` | the pending approval/question count |

None of these fields needs the owner token.

## Example: send a turn and follow the events

```sh
# One socket per hosting process: $RADIAN_HOME/run/<pid>.sock. Pick the hosting process
# you want; `$$` is this shell's pid, not the hosting process's.
pid="$(ls "$RADIAN_HOME/run" | sed 's/\.sock$//' | head -n 1)"
sock="$RADIAN_HOME/run/$pid.sock"
printf '%s\n' '{"id":1,"verb":"subscribe"}' | nc -U "$sock"
printf '%s\n' '{"id":2,"verb":"followup","text":"run the tests"}' | nc -U "$sock"
```
