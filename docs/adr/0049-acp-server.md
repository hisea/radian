# ADR 0049: The ACP Server (`radian acp`)

Status: accepted

## Decisions

### 1. A built-in subcommand and a third host surface

`radian acp` is a subcommand of the one `radian` binary (ADR 0032). It is a third host
surface beside the full-screen TUI and `radian exec`: it establishes the session through
`radian_host::open_session` and uses the same startup, model, permission, sandbox, MCP,
session and journal code. It changes nothing in `radian-core`.

The transport is newline-delimited JSON-RPC 2.0 over stdin and stdout
([Agent Client Protocol](https://agentclientprotocol.com), protocol version 1). stdout
carries protocol messages only; every diagnostic, startup line and error stays on stderr.

The JSON-RPC and ACP wire layer is written in `radian-exec` rather than taken from the
`agent-client-protocol` crate: the published crate is not version 1, and the host needs
only a small, closed subset of the protocol. The wire names, field spelling and enum values
follow the protocol schema exactly.

### 2. The flags are `radian exec`'s, without the line prompt

The conversation-shaping flags are one parser type shared by both commands, so they cannot
drift: `--model`, `--provider`, `--base-url`, `--config`, `--max-tokens`, `--thinking`,
`--system`, `--system-file`, `--mcp-config`, `--no-config-mcp`, `--workspace`, `--profile`,
`--allow-shell-command`, `--approval`, `--header`, `--session-id`, `--resume`,
`--session-dir`, `--include-usage`, `--max-model-calls`, `--turn-timeout-secs`,
`--context-window`, `--offline-catalog`, `--catalog-provider`, `--no-auto-compact`,
`--trust`, `--no-trust`, and the hidden internal flags `--parent`, `--parent-seq`,
`--tools`, `--deny-read` and `--tool-cancel-grace-ms`.

The line-prompt and print-and-exit inputs are absent: `-p`/`--prompt`, `--image`,
`--output`, `--yes`, `--list`, `--export`, `--search`, `--fork`, `--at`, `--format`,
`--out`, `--force`, `--include-reasoning` and `--include-provider-payload`. Input arrives as
`session/prompt`, output goes over the protocol, and approvals go over the protocol. The
argument string an arc wrapper builds for a `radian exec` worker is therefore accepted
unchanged once `-p -`, `--output jsonl` and `--yes` are removed.

### 3. One process holds one session

The first `session/new` creates the session. Its `cwd` must be an absolute directory that
equals `--workspace`; when `--workspace` is absent the request's `cwd` becomes the
workspace. The response carries the process's session id (`--resume`, else `--session-id`,
else a generated id), and every later session-scoped request must name it.

A second `session/new`, and a `session/new` whose `cwd` differs from the workspace, are
refused with an error that says one process holds one session. The same refusal covers a
`session/load` after either call, a second `session/load`, and a `session/load` naming a
different id than `--resume`. `initialize` advertises `loadSession: true`.

With `--resume` the first `session/new` opens that existing session, exactly as
`radian exec --resume` does. A missing journal is a `session/new` error. `session/load`
(decision 11) is the other way to open an existing session.

### 4. A prompt is one turn

`session/prompt` content enters the inbox as a user message with `InputSource::User`
(ADR 0011). Text content blocks are accepted; a resource link becomes a text line naming its
URI and label. Other content blocks are refused with an invalid-parameters error because
this host advertises no image, audio or embedded-context capability.

The response is sent after the driver has **journaled** the turn end
(`RuntimeEvent::TurnFinished`), not when the core's live `RunFinished` fires: the journal then
holds the turn end, so a client that reads the session file after the answer sees it, and the
`_meta` cost and text are folded from that turn's records only.

The turn end is the end of **the turn that claimed this prompt's message**, which
`RuntimeEvent::InputClaimed` names: a prompt is answered by its own turn and by no other. A
background job from an earlier prompt can settle and open a system wake turn (ADR 0037) between
the prompt entering the inbox and the driver taking it; that turn's end answers nothing, and the
prompt keeps waiting for the turn that drains its message. When the prompt's message is merged
into a turn together with other queued input, that turn is its turn. That turn's live
`RunFinished` supplies the status for `stopReason` — never another turn's:

| Turn end | `stopReason` |
|---|---|
| `RunStatus::Completed` | `end_turn` |
| `RunStatus::Cancelled(CancelReason::Requested)` | `cancelled` |
| `RunStatus::Incomplete(IncompleteReason::MaxTokens)` | `max_tokens` |
| `RunStatus::BudgetExceeded` | `max_turn_requests` |
| `RunStatus::Failed`, `RunStatus::Refused`, `RunStatus::Poisoned`, another incomplete reason | `refusal` |

A model or hook error that ends the turn (ADR 0017) is `refusal` with the error text in
`_meta.radian.error`. `max_tokens` means the turn ended on a truncated response with no tool calls
after two truncated responses in a row had already continued it (ADR 0017 §2). A truncated response
with tool calls continues the turn and never produces `max_tokens` by itself. A second `session/prompt` while a turn or a wake turn is running is
refused with an error. `session/cancel` calls the inbox's requested cancellation, the same
interruption an ESC produces in the TUI, and the journal keeps the interrupted prefix
(ADR 0003). A `cancelled` answer to a permission request also cancels the turn.

Background jobs are not awaited: the prompt response reports the turn. A later job-completion
wake turn streams its `session/update` notifications in the same session.

### 5. Live events become `session/update`

Live events (ADR 0010) are forwarded one for one; durable events go only to the journal and
are replayable by a TUI exactly as `radian exec` writes them.

| Live event | `session/update` |
|---|---|
| `ModelTextDelta` | `agent_message_chunk` with a text content block |
| `ModelReasoningDelta` | `agent_thought_chunk` with a text content block (ADR 0014) |
| `ToolStarted` | `tool_call` with the tool-call id, a title, a kind, `in_progress`, the locations from the arguments, and `rawInput` |
| `ToolFinished` | `tool_call_update` with the status `completed` or `failed` |
| the committed `Tool` message | `tool_call_update` with `rawOutput` (the structured output, else the result text) |
| `ModelResponseReceived` with usage, and `CompactionUsage` | `usage_update` with the used tokens and the context window, when the window is known (ADR 0007) |

Titles, kinds and locations are derived from the built-in tool name and arguments. A tool
whose arguments name a path gets that resolved path as its location.

### 6. `_meta` carries what the protocol lacks

The `session/prompt` response carries `_meta.radian`:

- `cost`: the fee of the turn that answered the prompt, in the object `radian exec`'s
  `Result.cost` carries, `{ "micros", "incomplete", "estimated" }`, or `null` when no call of
  that turn had a price. `micros` and `incomplete` are folded from that turn's records only;
  `estimated` is `false`, because every call of a turn this process ran was priced as it
  arrived (ADR 0027), never by re-pricing restored history.
- `text`: the last assistant message's text in full **of the turn that answered the prompt**,
  or empty when that turn appended none.
- `error`: present only on a `refusal` that a model or hook error of that turn caused; the
  masked error text.

A client that does not read `_meta` ignores it.

### 7. Approvals and `ask_user` go through `session/request_permission`

Every ADR 0024 permission question and every `ask_user` question reaches the client as
`session/request_permission`. The host uses the session's pending queue (ADR 0023 U7); it
turns the queue head into one protocol request and answers the queue when the client
answers. The request's `toolCall` names the tool, the resolved resource, the raw arguments,
the status `pending` and the locations.

- A permission question offers `allow_once` (`AllowOnce`), `allow_always`
  (`AllowForSession`) and `reject_once` (a denial whose reason reaches the tool result).
- `allow_always` is the ADR 0012 `AllowForSession` grant. It lives in this process only: it
  is not written to a profile, it is not restored after a restart, and a resume has none.
- A `cancelled` outcome answers the pending request with a denial carrying that reason and
  cancels the turn; the tool result and the journal follow the interrupted-turn rule
  (ADR 0003).
- An `ask_user` request is asked one question at a time. A question's options become the
  permission options, and a selected option becomes the answer. A "Decline" option maps to
  `PromptError::Declined`. `request_permission` carries one selection and no free text, so a
  `text` question, a free-text `Other` answer and a multi-selection answer are not available
  on this surface; a text question fails the tool with `prompter_error` instead of inventing
  an answer.

### 8. The trust gate is non-interactive

Like `radian exec` with piped input, `radian acp` never asks the startup trust question. An
untrusted workspace loads an untrusted project, the status line goes to stderr, and the
one-line notice names the way out. `--trust` and `--no-trust` still resolve for this process
as they do in `radian exec` (ADR 0031).

### 9. `mcpServers` are stdio servers at the `--mcp-config` level

The `mcpServers` of `session/new` are merged as this session's MCP configuration at the
same precedence level as `--mcp-config`; on a duplicate id the client's entry wins, and the
collision is one warning line on stderr. Only stdio servers are accepted. An HTTP, SSE or
ACP server is refused in the `session/new` error, which names the transport as unsupported.

### 10. No client filesystem or terminal calls

The host never calls `fs/read_text_file`, `fs/write_text_file` or `terminal/*`. radian reads
and writes files with its own tools and starts its own shell, under the same sandbox and
permission rules as every other host (ADR 0016, ADR 0020, ADR 0039). The protocol response
advertises no client capability dependence.

### 11. `session/load` replays the journal and continues it

`session/load` opens an existing session by id through `--resume`'s lookup: the same journal,
the same recovery, the same model route and access the journal records. Its `cwd` obeys the
`session/new` rule (an absolute directory equal to `--workspace`, or the workspace when the
flag is absent), and its `mcpServers` are merged exactly as `session/new`'s are (decision 9).
The named session must belong to that workspace: its `SessionCreated.workspace` must equal the
process workspace, the rule `--export` applies. Both values are canonical paths (the host
resolves both with `resolve_workspace`), so a workspace reached through a symlink matches the
directory it names. A session created in another workspace is refused with an
invalid-parameters error that names it, before the session is opened, so the refusal writes
nothing and the process can still load a session of its own workspace or start one with
`session/new`. An unknown id is an error from the opener that names the id, and a file whose
`SessionCreated` cannot be read is that opener's error too, never this refusal.

The response is an empty `LoadSessionResponse`. Before it, the journal's transcript — the
recovered history `radian exec --resume` would send to the model (ADR 0002 repairs an unpaired
call) — is replayed as `session/update` notifications in transcript order:

| Transcript | `session/update` |
|---|---|
| a user message's text | `user_message_chunk` |
| an assistant message's reasoning | `agent_thought_chunk` |
| an assistant message's text | `agent_message_chunk` |
| each tool call of an assistant message | `tool_call`, as decision 5 describes it |
| the committed tool result | `tool_call_update` with `completed` or `failed` and `rawOutput` |

The replay is a read: it writes nothing to the journal. A prompt after it is the ordinary
`session/prompt` of decision 4 and continues the **same** journal — a load is a resume, never a
fork (ADR 0006, ADR 0022). The session id stays the loaded one. A resource link is replayed as
the `[name](uri)` line a prompt would have produced; an image, an audio block or an embedded
resource has no text form on this surface and is not replayed. The protocol has one text
channel for a user-role message, so a host-injected one (a memory block or a job notice,
ADR 0011) is replayed the same way.

The `allow_always` grant of decision 7 lives in the process that answered it, so a loaded
session has none: the first call that needs an approval asks again.

## Constraints and Costs

- One prompt runs at a time per process; parallel work belongs to arc's processes or to the
  `subagent` tool, not to one ACP connection.
- `request_permission` is a lossy channel for structured questions: free text and
  multi-selection are not representable.
- The hand-written wire layer covers the methods above; an unimplemented method is a
  JSON-RPC method-not-found error, never a silent success.
- The ACP host binds no control socket: its client is the only control surface.

## Verification

`crates/radian-exec/tests/acp.rs` drives `radian acp` as a child over stdio: initialize,
`session/new`, `session/prompt`, the update stream, the prompt `_meta`, a permission
rejection, a cancellation with the interrupted journal prefix, a model error, a prompt whose
journaled turn end is delayed (the answer must wait for it), a prompt whose own turn follows a
job-completion wake turn (the wake turn's end must not answer it, and the answer reports that
turn's own cost and text, not the session's), the one-session refusal, a `session/load` of a
session created in another workspace (refused by name, then a load of a session in the process
workspace succeeds), the
shared flag set, and `session/load` — a session written by one process and loaded by a second,
whose replay and continued journal the test checks by folding the journal. `journal-selfcheck`
replays the sessions the suite writes. The touched crates' mutation records name the tests that
catch a cancelled turn answering `end_turn`, a second `session/new` being accepted, a
`session/load` that starts a new journal instead of continuing the loaded one, a prompt answer
that follows the live `RunFinished` instead of the journaled turn end, and a prompt answered by
the wake turn that ran before its own.
