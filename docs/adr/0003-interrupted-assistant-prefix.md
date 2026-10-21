# ADR 0003: Interrupted Assistant Prefix

Status: accepted

## Background

When cancellation interrupts a streaming model call, previously generated text was lost in full: neither memory nor journal held it. The next request had to restart from the last user message. Core therefore commits the text and reasoning observed through the event path as one `interrupted` assistant message (journal first, then memory) when cancellation interrupts a model call. The conversation can then continue.

## Decisions

### F1: `Message::Assistant` carries `interrupted: bool`

`#[serde(default, skip_serializing_if = "std::ops::Not::not")]`: the field defaults to false, and false does not serialize. Old JSON without the field deserializes as false. Constructors set false.

### F2: Core accumulates what it receives through the event path as `ModelTextDelta` and `ModelReasoningDelta`

The source is not "what the screen draws" (the host UI can truncate, merge, or drop deltas). The source is also not the adapter-internal buffer (it can hold unflushed deltas that never reach the host). Implementation: `step_once` creates one new `StreamTap` event wrapper per model call. The wrapper forwards each event to the host sink and appends the two delta types to two buffers. **Each call creates a new tap**, so text from one call never leaks into the next.

### F3: Commit point and commit path

Core commits after `race(&interrupt, model.complete(..))` returns `Interrupted` (the model future is already dropped, then Core reads the accumulators) and before `finish(Cancelled)`. When both buffers are empty, Core commits nothing. The commit uses the single ADR 0001 `commit` path (journal → memory → event). The append is not cancellable (ADR 0001 D3). A failure ends with `RunError::Journal` plus `RecoveryRequired`.

### F4: "Empty" means both visible text and reasoning are empty

Text with only whitespace still commits: the check uses `is_empty()`, not `trim().is_empty()`. Core does not decide what counts as "meaningful content" for the model or the host. When either buffer is non-empty, Core commits one `interrupted: true` assistant message. The two buffers enter `Message::Assistant.content` and `.reasoning` respectively. Reasoning-only progress would otherwise be lost: a reasoning model sends `""` in `delta.content` during the whole reasoning phase, so a visible-text-only check would drop the entire reasoning phase.

### F5: Shape of the committed message

`Message::Assistant { content: [accumulated text], tool_calls: [], provider_payload: None, reasoning: [accumulated reasoning], interrupted: true }`. It **never carries tool calls**: a half-formed tool argument cannot be valid, and the event path never carries it. The `after_model_response` hook does **not** run for it. It is not a complete response.

### F6: Core commits only when cancellation interrupts a model call

Core does not commit when: the model returns `Err` (even with deltas present); the `step()` future is dropped (poison; the synchronous path cannot touch the journal, ADR 0001 D7); cancellation fires outside a model call (before/after hook, tool phase).

### F7: Visible difference in `final_message`

After the commit, `RunStatus::Cancelled(reason)` does not change. `RunOutcome.final_message` still follows the existing logic and takes the last assistant message in the turn, which **can be this prefix**. The host must check `interrupted` before it displays "the final answer", or it shows a half sentence as a complete answer. Tests pin this behavior.

### F8: The adapter treats it as a normal assistant history message

The OpenAI-compatible adapter sends no new field: content text, no tool calls. The request body is byte-identical to the same content without `interrupted`. The model sees only "words I said before". ADR 0014 defines the reasoning pass-back rules.

### F9: SIGKILL does not guarantee recovery of an uncommitted prefix

Only the cancellation path commits. Text accumulated in flight is lost when SIGKILL kills the process (the journal holds nothing). That boundary is the honest promise. The ADR 0002 marker rules do not cover streaming intermediate state.

## Crash and interrupt semantics table (during a model call)

| Interrupt type | Prefix destination |
|---|---|
| User cancellation / deadline (`race` Interrupted) | Commit (this document) |
| Model `Err` | No commit (F6) |
| `step()` future dropped (poison) | No commit; the synchronous path does not touch the journal |
| SIGKILL / crash | No guarantee (F9) |

## Constraints and costs

- `StreamTap` adds one `Mutex` lock plus one forward per event for each model call. Events already form a short synchronous path, so the cost is negligible.
- The `final_message` ambiguity (it can be the prefix) is resolved by the host check on `interrupted`. Core adds no separate field for it.
- "What Core sent" and "what the user saw" can differ (when the host UI drops frames, the journal can hold more). The Core-observed version is the verifiable choice.

## Verification

`crates/agent-core/tests/prefix.rs` covers cancellation with text only, reasoning only, both, neither, fresh accumulation per call, and the `interrupted` check on `final_message`.
