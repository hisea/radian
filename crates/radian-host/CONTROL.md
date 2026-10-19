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

`followup`, `steer` and `inject` set the turn's source; the client picks it with an optional
`source` field:

| `source` | result |
|---|---|
| absent, `"scheduler"` | `InputSource::Scheduler` (the default) |
| `"subagent"` | `InputSource::Subagent` |
| `"mcp"` | `InputSource::Mcp` |
| `"user"` | **refused** — the port never accepts `User` (ADR 0043) |
| `"permission_decision"` | **refused** |
| anything else | refused |

Approvals cannot be answered over the port: there is deliberately no approval or answer verb,
and an unknown verb (including `"approve"`) returns an error and changes nothing.

### Verbs

| verb | fields | result |
|---|---|---|
| `followup` | `text`, `source?` | enqueues a new turn; `message_id` |
| `steer` | `text`, `source?` | redirects the running turn |
| `inject` | `text`, `source?` | adds context without waking an idle session |
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
