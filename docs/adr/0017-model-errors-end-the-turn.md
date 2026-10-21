# ADR 0017: Model and Hook Errors End the Turn

Status: accepted

## Decisions

### 1. Error classes

| `RunError` | Handling | Reason |
|---|---|---|
| `Model(..)` (HTTP errors, broken streams, idle timeouts, malformed streams, cancellation) | **Non-fatal**: this turn ends as `failed`, the driver continues | Retryable; session state is intact |
| `Journal(..)` (including `RecoveryRequired`) | **Fatal**: the driver ends | Memory and journal can diverge (ADR 0001 D6) |
| `Poisoned` (an in-flight future dropped) | **Fatal**: the driver ends | The session cannot continue |
| `Session(..)` (tool session, illegal history) | **Fatal**: the driver ends | The session itself is unusable |
| `Hook { .. }` (`before_model_call` / `after_model_response`) | **Non-fatal**: this turn ends as `failed`, the driver continues | Session state is intact, the transcript is legal |

Model cancellation (`ModelError::Transport("the request was cancelled")`) belongs here: Core already ends the turn as `Cancelled`, so it never becomes `RunError::Model`.

### 2. Bounded retries for transient errors first

- **Only transient errors retry**: connection failures, streams ending before response completion, idle timeouts, HTTP 408/429/500/502/503/504/529 (plus errors a provider explicitly marks overloaded), and **empty responses** (see below). 400/401/403/404, over-long context (ADR 0018 handles it separately), malformed streams, and cancellation **never** retry. Classification lives on the adapter error type (`is_transient()`). Both adapters implement it with tests. The host never matches error text.
- **Empty responses retry as transient model errors**: one model response is an **empty response** when it meets all of these at once. It carries no tool call. `stop_reason == EndTurn`. Every visible text block is empty (`is_empty()`, not `trim()`, same caliber as ADR 0003 F4; reasoning does not count: a reasoning-only answer reads as empty to the user; a non-text block such as image or opaque counts as content: its presence means non-empty). That is an empty response (`ModelError::EmptyResponse`, `is_transient()` true). Blank text (`"  "`) is not empty. Core never judges what counts as "meaningful content" for the model. A response with tool calls dispatches per tool semantics as usual. Other stop reasons such as `MaxTokens` keep existing handling. An empty response in an unattended run is a false success (no tools, no text, yet ends as `Completed`). So it takes the same retry path: same `retries` count, same wait cap, same cancel-during-backoff semantics, same note on exhaustion ("after N retries"). It finally ends as `failed`, never `Completed`.
- **The empty check lives only in core, never in adapters**: `model_call_with_retries` converts an empty `Ok(response)` landing into an `EmptyResponse` error into the same retry branch (call sites opt in). It runs before `ModelResponseReceived`, hooks, and commit. So the empty attempt commits no assistant message, writes no usage, and sends no response event. The exhausted final attempt behaves the same (a failed attempt never commits). The check point lives inside one model call. It covers dialogue calls only. Compaction summary calls have no such protection. ADR 0018 handles empty summaries itself.
- **Policy**: default 3 retries. Backoff is 2 s, 4 s, 8 s (plus small jitter). With `Retry-After`, honor it, but cap one wait at 60 s and all waits combined at 60 s. Configuration is `[model] retries = 3`. `0` disables. TUI and `radian exec` share the default.
- **Layer**: inside one core model call, **before that call commits its result**. The failed attempt already streamed visible text and reasoning. They drop (they are live projections, ADR 0010; they never enter the journal). Core sends one live event `ModelRetrying { attempt, max, delay, reason }`. TUI clears the streaming block from it and shows one line `retrying 1/3 in 4s: <reason>`. `radian exec` prints the same line on stderr.
- **Cancellation**: Esc or Ctrl-C during backoff ends at once. Cancellation semantics apply (this turn cancelled). No more retries run.
- **Exhausted retries**: end as `failed` per §3. `TurnFailed.message` appends "after N retries". When the wait budget (60 s combined) runs out, the code never retries again with 0-second backoff. It ends the turn directly.
- **Billing and journal**: when the provider reports usage for failed attempts, the code records it (real cost). The successful attempt commits as usual. Retries add no durable records. Current adapter error types carry no usage. So failed attempts write no usage. Only the successful attempt records as usual. **Discarded empty-response attempts also write no usage**: a usage record (J5) must directly follow its assistant message. With no message to follow, no record exists to write. This is a cost of the current shape, not a core choice.
- **ChatGPT plan window exhausted**: when upstream returns 429 but body `error.type == "usage_limit_reached"`, retries cannot succeed before the window resets. It classifies as a `Protocol` error and never retries. The error message states the plan, the window, and the reset time, and points at `/model`. Any other 429 retries as usual.

### 3. Disk records and the host channel

On a `Model` or `Hook` error the driver writes two runtime events:

1. `RuntimeEvent::TurnFailed { turn_id, message }`. `critical: false` (audit, no state rebuild). `message` is the adapter error text (already masked).
2. `RuntimeEvent::TurnFinished { turn_id, outcome: failed, .. }`. End-state record, same shape as a successful turn.

The driver adds `pub trait RuntimeEvents` (`Fn(&RuntimeEvent)` blanket impl) and `Driver::set_runtime_events(Arc<dyn RuntimeEvents>)`. It also calls the listener when it writes `TurnFailed`. The host redacts once more in its own closure with its own secrets list, then displays. Choice of listener over journal polling: errors must display at once when they happen. Choice of callback over a new `EventSink` method: the latter is a Core API change.

### 4. Hook error transcript state

`before_tool_call` and `after_tool_call` errors are always **fail closed**. The call gets a `hook_error` result and continues. They **never end the turn**, so they never produce `RunError::Hook`. Point by point:

| Hook point | Behavior on failure | Unpaired calls left |
|---|---|---|
| `before_model_call` | `fail_hook` ends this turn as `failed` | No. It runs before the model request: this step has no assistant message yet. The last step tool calls already resolved in the same step |
| `after_model_response` | `fail_hook` ends this turn as `failed` | No. It runs before the assistant message `commit`. The response drops |
| `before_tool_call` | **Never fails the turn**: it commits a `hook_error` result into the transcript at once and continues | No |
| `after_tool_call` | **Never fails the turn**: it replaces the result with `hook_error`, then commits, and continues | No |
| `on_turn_end` | No return value | — |

So only `before_model_call` and `after_model_response` produce `RunError::Hook`. Neither leaves unpaired calls. No poison-style result patching is needed.

### 5. Host behavior

- **TUI**: shows `turn failed: <message>` as one transcript notice, returns to input state, never exits the session.
- **CLI interactive mode**: errors go to stderr, the prompt returns, reading continues on the next line.
- **CLI `-p`**: still ends with exit code 1 (it is one turn by nature).

## Constraints and costs

- Retry policy configures from the host and executes inside one core model call: only core knows that this call committed nothing yet.
- The failed attempt streaming text drops by necessity (otherwise it repeats). Cost: the user can see content flash then vanish.

## Verification

`radian-runtime::provider_failure` covers a failed turn writing `TurnFinished`, a successful next turn, and resume history consistency. `hook_failure` covers hook errors ending the turn while the session continues. `agent-core::hooks` covers a pre-model hook failure with a legal transcript and `before_tool_call` fail-closed. TUI and CLI pty and cli tests cover error display and `-p` exit code 1. Retry tests cover success after two broken streams, no retry on 400, 429 per `Retry-After`, cancel during backoff, `retries = 0`, the wait cap, and no retry on usage-limit. Both adapters carry `is_transient` classification tests. Both provider contracts stay green.
