# ADR 0002: Tool Execution Markers and Recovery Rules

Status: accepted

## Background

The "journal before memory change" seam (ADR 0001) does not answer three questions after a tool batch crashes: the assistant message is journaled — did the call dispatch? Did it run? What result did it produce? Without answers to these three questions, recovery can only say "unknown" in general. Two execution markers in the journal plus pure-function rules from markers to recovery results answer the questions.

## Decisions

### E1: Two records

`JournalRecord` carries `ToolExecutionStarted { call_id, name, arguments }` and `ToolExecutionSettled { call_id, result }` (the enum is already `#[non_exhaustive]`). `arguments` holds the post-`Rewrite` parameters **actually passed to the tool**. `result` holds the **final** result after `after_tool_call` that will enter the transcript (including `cancelled` and `hook_error` results). `Settled` carries the full result, so a crash point "settled but the result message is not journaled" still rebuilds without loss.

### E2: No separate record for approval decisions

An allowed or rewritten call leaves its evidence in `Started.arguments` (post-rewrite). A rejected, `Respond`, `hook_error`, or preflight-failed call **never executes**. Its synthetic or response result message is itself the evidence. It carries **no** `Started`/`Settled` marker. Rationale: a crash "after approval but before Started" and "never approved" lead to the same recovery conclusion (the call never ran). One more record would add state without changing the decision.

**Accepted audit loss**: who approved what, when, with which reason, has no independent durable record. The host can only infer "this call was allowed" from the presence of `Started` plus its parameters, and "this call was rejected" from the `denied`/`hook_error` text in the result message. A host that needs a complete approval audit keeps its own record (in the hooks layer, outside Core journal semantics).

### E3: Order

Each executed call follows this order: assistant message is journaled → `before_tool_call` → **`Started` append succeeds** → dispatch (enter running) → tool plus `after_tool_call` finish → **`Settled` append succeeds** → result enters the prefix commit in call order (through the `commit_parts` from ADR 0001). Core awaits each `Started` before it places that call into `running`. Core awaits each `Settled` when that call future returns and before `resolve`. This order guarantees "the tool body starts to execute only after `Started` is durable".

### E4: Failure semantics

`Started` append fails: the call does **not** dispatch, the turn ends with `RunError::Journal`, and the session enters `RecoveryRequired`. The batch stops at once. Already queued running futures that were never polled are dropped with the batch (they never execute and have no side effects; if their `Started` records are already written, recovery treats them conservatively as "Started only"). `Settled` append fails: the call has already executed once, and the batch dispatches no further call. The turn ends with `RunError::Journal`.

When a batch stops because of a journal failure, running calls **receive a cancellation signal and a grace period**. Core discards their results and writes no `Settled` for them (the journal is already untrusted, so Core appends nothing more). Recovery treats them as "`Started` only". Calls already pushed into `running` but never polled **do not start the tool body** after cancellation fires: `call_one` checks cancellation at entry and returns `cancelled` directly. Core sends the `ToolStarted` event only after the `Started` append succeeds.

### E5: Cancellation also settles

A cancelled tool (whether it finishes itself inside the grace period or is dropped after the grace period) still writes `Settled { result: cancelled }`. Cancellation is not "it never ran". As in ADR 0001 D3: Core does not cancel an in-flight append.

### E6: Drop and poison paths write no markers

The synchronous drop path (ADR 0001 D7) cannot await, so it writes no `Settled`. The replacement `interrupted` result in memory stays in memory only. In the journal, that call is "`Started` only" (or has no marker). Recovery applies the rules. The `journal-selfcheck` prefix assertion still holds for the new records (the comparison reads only `Message` records).

### E7: Two recovery-only error codes

`not_started_after_crash` ("the call never started to execute") and `outcome_unknown_after_crash` ("the tool may already have executed; the result is unknown; do not assume it did not execute"). Core adds no field to `ToolResult`.

### E8: Three pure functions (no I/O, no tool contact)

- `replay(records) -> Result<Vec<Message>, ReplayError>`: rebuilds `Message` records in order. Indices must run continuously from 0. It ignores marker records.
- `repair(history, records) -> Vec<Message>`: for each unpaired call in history, returns the result messages **to append**, per the crash semantics table (in assistant call order).
- `recover(records) = replay + repair`.

### E9: The host writes the repair back before it reopens

The host must first write the `repair` messages **into the journal** (as `Message` records), then open with `open_session_with_journal(history, …)`. Per ADR 0001 D8, history passed at open time counts as already durable. Otherwise journal and memory diverge at once after reopen. Core provides no automatic write-back helper.

## Crash semantics table (per call)

| Crash position (for that call) | Journal state | Recovery result |
|---|---|---|
| Assistant message not journaled | No record from that batch | `not_started_after_crash` |
| Assistant journaled, call not approved or not dispatched | No `Started` | `not_started_after_crash` |
| `Started` journaled, tool body never polled because the future was dropped | `Started` only | `outcome_unknown_after_crash` (conservative: the tool may still have run) |
| `Started` journaled, tool running or already run | `Started` only | `outcome_unknown_after_crash` (side effects may exist) |
| `Settled` journaled, result message not journaled | `Started` plus `Settled` | Rebuild `Settled.result` unchanged |
| Result message journaled | Complete | Unchanged (already paired, `repair` does not touch it) |
| Unexecuted call (denied / preflight failed / Respond / hook_error / skipped / never started before cancellation) | No marker, result message present | Unchanged (already paired) |

Meaning for the host: `outcome_unknown_after_crash` means **side effects may exist**. The host must not retry automatically. It passes the fact to the model (or the user) for a decision. `not_started_after_crash` is safe to resend.

## Constraints and costs

- `run_group` awaits each `Started` per call and each `Settled` per settlement. While it awaits inside a `select!` branch, other branches pause polling. Parallel batches still complete in full, stay ordered, and never deadlock (tests prove this property).
- A `Started` write can succeed while the future is dropped before its first poll because a later failure stops the batch. The journal then holds "Started only" for a call that never executed. Recovery leans conservative (unknown). It is safe but can cost the model one more question.
- Calls in the later part of a failed batch have no results in the in-memory transcript (ADR 0001 D6). Only `recover` produces a legal history. That behavior is by design, not a defect.

## Verification

`crates/radian-core/tests/journal.rs` and `tests/replace.rs` cover every row of the crash semantics table, the E2 no-marker paths, the E4 cancellation plus grace period, and the three pure functions in E8 plus `journal-selfcheck`.
