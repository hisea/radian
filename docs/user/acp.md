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
- `session/load` is not supported in this version: `initialize` advertises
  `loadSession: false`, and a `session/load` request is refused.

## Methods

| Method | What happens |
|---|---|
| `initialize` | Negotiates protocol version 1 and advertises `loadSession: false`. |
| `session/new` | Creates (or, with `--resume`, continues) the one session; see above. |
| `session/prompt` | Sends the prompt's text as a user message (a resource link becomes a text line) and answers when the turn ends. |
| `session/cancel` | Interrupts the running turn, like ESC in the full-screen interface; the journal keeps the interrupted prefix. |
| `session/request_permission` | Sent by radian for every approval and every `ask_user` question. |
| `session/update` | Sent by radian for the live turn; see below. |

`session/prompt` answers with `stopReason`: `end_turn`, `cancelled`, `max_tokens`,
`max_turn_requests` (the model-call budget ran out) or `refusal` (a model or hook error, or
another unfinished answer). A second prompt while a turn runs is refused. The response's
`_meta.radian` carries what ACP has no field for:

| Field | Meaning |
|---|---|
| `cost` | `{ "micros", "incomplete", "estimated" }`, the same object `radian exec`'s `Result.cost` carries, or `null` when no call has a price. |
| `text` | The last assistant message's text, in full. |
| `error` | Present only on a `refusal` caused by a model or hook error: the masked error text. |

A client that does not read `_meta` ignores it.

## Updates

`session/update` carries only live events; the durable transcript goes to the session journal
and a full-screen session can resume it.

- `agent_message_chunk`: streamed answer text.
- `agent_thought_chunk`: streamed reasoning.
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
