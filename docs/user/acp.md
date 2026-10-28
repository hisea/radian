---
description: Serve radian to an editor over the Agent Client Protocol (stdio), with one session per process.
---

# `radian acp`: the Agent Client Protocol server

`radian acp` is the same coding agent as the full-screen interface and `radian exec`,
exposed over [Agent Client Protocol](https://agentclientprotocol.com) (ACP) version 1:
newline-delimited JSON-RPC 2.0 on stdin and stdout. The client starts it as a child process,
sends prompts, answers permission requests, and receives the turn as `session/update`
notifications. stdout carries protocol messages only; startup lines, warnings and errors go
to stderr.

```sh
radian acp --provider openai-compatible --model gpt-5 --workspace "$PWD" --profile workspace
```

The key comes from the credential store, never from the command line, exactly as in every
other host: run `radian login` first (see [`providers`](providers.md)). There is no ACP
authentication method in this version.

## The flags

`radian acp` accepts the conversation flags of [`radian exec`](cli.md#radian-exec-the-headless-command) with the
same names, values and defaults. Use that page for every one of them; this page does not
repeat their meaning.

The line-prompt and print-and-exit inputs are **absent**: `-p`/`--prompt`, `--image`,
`--output`, `--yes`, `--list`, `--export`, `--search`, `--fork`, `--at`, `--format`,
`--out`, `--force`, `--include-reasoning` and `--include-provider-payload`. Input arrives as
`session/prompt`, output goes over the protocol, and approvals go over the protocol. The
flag string a wrapper builds for `radian exec` is accepted unchanged once `-p -`,
`--output jsonl` and `--yes` are removed.

## One process, one session

- The first `session/new` creates the session. Its `cwd` must be an absolute directory. If
  `--workspace` was given, `cwd` must name the same directory; if it was absent, the
  request's `cwd` becomes the workspace.
- The response is `{"sessionId": "<id>"}`. That id is `--resume`, else `--session-id`, else a
  generated id, exactly as in `radian exec`.
- A second `session/new`, and a `session/new` whose `cwd` differs from `--workspace`, fail
  with an error that says one process holds one session.
- `--resume <id>` makes the first `session/new` continue that journal. A missing journal is a
  `session/new` error.

## Loading an existing session

`session/load` opens an existing session by id, the same lookup `--resume` uses, and replays
its transcript to the client. `initialize` advertises `loadSession: true`.

- The request's `cwd` must satisfy the same rule as `session/new`: an absolute directory that
  equals `--workspace`, or, when `--workspace` is absent, the workspace itself.
- The reply is an empty object. Before it, radian sends the journal's transcript as
  `session/update` notifications, in transcript order: `user_message_chunk` for each user
  message, `agent_thought_chunk` for reasoning, `agent_message_chunk` for answer text, and a
  `tool_call` followed by the `tool_call_update` of its final status (`completed` or `failed`,
  with the tool's output) for each tool call.
- A prompt after the load continues the **same** journal: the loaded session is continued, never
  forked, exactly like `radian exec --resume`. The session id stays the one that was loaded.
- An unknown id is an error that names the id.
- A `session/load` after `session/new`, a second `session/load`, and a `session/load` naming a
  different id than `--resume` are refused with the one-process-one-session error.
- An `allow_always` grant from an earlier process is **not** restored: the first call that needs
  an approval after a load asks again.

## Methods

| Method | What happens |
|---|---|
| `initialize` | Negotiates protocol version 1 and advertises `loadSession: true`. |
| `session/new` | Creates (or, with `--resume`, continues) the one session; see above. |
| `session/load` | Opens an existing session by id, replays its transcript as updates, and continues it; see above. |
| `session/prompt` | Sends the prompt's text as a user message (a resource link becomes a text line) and answers once the end of **its own** turn is written to the session file. |
| `session/cancel` | Interrupts the running turn, like ESC in the full-screen interface; the journal keeps the interrupted prefix. |
| `session/request_permission` | Sent by radian for every approval and every `ask_user` question. |
| `session/update` | Sent by radian for the live turn and for a `session/load` replay; see below. |

`session/prompt` answers with `stopReason`: `end_turn`, `cancelled`, `max_tokens`,
`max_turn_requests` (the model-call budget ran out) or `refusal` (a model or hook error, or
another unfinished answer). `max_tokens` means the turn ended `Incomplete` for a truncated
answer that could not continue; see [the exit-code rule](cli.md#a-truncated-answer) for when that
happens. The answer waits for the driver to journal the turn end, so a client that reads the
session file right after the response already sees it. The turn is the one that took the prompt's
message: a background job's completion notice can wake a turn of its own before that turn runs,
and the wake turn's end answers nothing — the prompt keeps waiting for its own turn's answer. A
second prompt while a turn runs is refused. The response's `_meta.radian`
carries what ACP has no field for:

| Field | Meaning |
|---|---|
| `cost` | `{ "micros", "incomplete", "estimated" }`, the fee of the turn that answered the prompt, or `null` when no call of that turn had a price. `estimated` is always `false`: a turn this process ran priced each call as it arrived. |
| `text` | The last assistant message's text, in full — for a prompt, the text of the turn that answered it; empty when that turn appended none. |
| `error` | Present only on a `refusal` caused by a model or hook error of that turn: the masked error text. |

A client that does not read `_meta` ignores it.

## Updates

`session/update` carries the live events of a turn, and the replayed transcript of a
`session/load`; the durable transcript goes to the session journal and a full-screen session can
resume it.

- `agent_message_chunk`: streamed answer text.
- `agent_thought_chunk`: streamed reasoning.
- `user_message_chunk`: a user message, in a replayed transcript.
- `tool_call`: a tool call started, with its id, title, kind, status, locations and raw input.
- `tool_call_update`: a tool finished, with its status and raw output.
- `usage_update`: the provider's reported usage and the context window, when the window is
  known.

## Approvals and `ask_user`

Every permission question and every `ask_user` question becomes `session/request_permission`:

- A permission question offers `allow_once`, `allow_always` and `reject_once`. `allow_always`
  is a grant for this process only: it is never written to a profile and a new process has
  none. A rejection is a tool result carrying the reason.
- A `cancelled` outcome denies the pending request with that reason and cancels the turn; the
  journal keeps the interrupted prefix.
- `ask_user` is asked one question at a time, and the question's options are the permission
  options. `request_permission` carries one selection and no free text, so a `text` question,
  a free-text `Other` answer and a multi-selection answer are not available on this surface;
  a text question fails the tool instead of inventing an answer.

## MCP servers

The `mcpServers` of `session/new` are merged as this session's MCP configuration at the same
level as [`--mcp-config`](cli.md#radian-exec-the-headless-command); a duplicate id keeps the client's entry and
prints one warning on stderr. Only stdio servers run. An HTTP, SSE or ACP server is refused
in the `session/new` error.

## Trust and file access

The startup trust question is never asked: an untrusted workspace loads an untrusted project
and prints the one-line notice, exactly as `radian exec` with piped input does. See
[`permissions`](permissions.md) for `--trust`, `--no-trust` and the trust file.

radian never calls the client's `fs/read_text_file`, `fs/write_text_file` or `terminal/*`.
It reads and writes files with its own tools and starts its own shell under the same sandbox
and permission rules as the other hosts.

## Limits

- One prompt runs at a time per process. Parallel work belongs to another process or to the
  `subagent` tool.
- Background jobs are not awaited: the prompt response reports the turn, and a later
  job-completion wake turn streams its updates in the same session.
- A method that this version does not implement is a JSON-RPC method-not-found error.
