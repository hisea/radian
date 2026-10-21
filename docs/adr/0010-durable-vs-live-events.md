# ADR 0010: Durable Versus Live Events

Status: accepted

## Background

Durable facts enter JSONL. Live projections do not enter JSONL and allow dropping, merging, and rate limiting. This ADR fixes the **decision criteria**, classifies each `AgentEvent` variant one by one, and states its relation to `JournalRecord`.

## Decisions

### K1: Decision criteria

- **(R1) Rebuild-required**: a fact **required** to rebuild the transcript, tool execution state, input and turn boundaries, configuration, or to audit "who approved what" → **durable**. It is either a Core `JournalRecord` or a runtime-level ADR 0006 JSONL event. Core owns transcript/tool/usage/compaction facts. Runtime owns input/turn/config/permission/lineage facts.
- **(R2) In-progress observation**: an event that describes "an observation in progress", and whose value is **not** required for rebuild, or is re-derivable from durable facts → **live projection**.

Dropping, merging, and rate limiting apply **only** to R2. R1 facts are never dropped. One exception exists in R2: `RunFinished` is the mandatory end state (K3).

Core `JournalRecord` is `#[non_exhaustive]`. Current variants: `Message`, `TranscriptReplaced`, `TranscriptReplacementCommitted`, `TranscriptReplacementAborted`, `ToolExecutionStarted`, `ToolExecutionSettled`, `ModelUsage`, `CompactionUsage`, `ContextCompacted`.

### K2: Classification of all `AgentEvent` variants

| `AgentEvent` variant | Class | Relation to durable records | Note |
|---|---|---|---|
| `TranscriptAppended { index, message }` | Delivery notice for a durable fact | One to one with `Message` | The record already carries the fact. The event only delivers it to the host. |
| `TranscriptReplaced { revision, len }` | Same as above | One to one with `TranscriptReplaced` | **Intentionally asymmetric**: each replacement message in the journal is an independent `Message` record, but Core sends **no** `TranscriptAppended` per message, only one `TranscriptReplaced`. |
| `ToolStarted { index, call_id, name }` | Same as above | One to one with `ToolExecutionStarted` (sent only after the `Started` append succeeds) | The audit receipt lives in the record. |
| `ToolFinished { index, call_id, is_error }` | Live | Durable counterpart is `ToolExecutionSettled` (cancellation also settles) | `is_error` is rebuildable from the result. |
| `ModelResponseReceived { stop_reason, tool_calls, usage, limits }` | Live plus partly durable | `usage` maps to the `ModelUsage` record. `limits` is live (limit headers are display facts). | The facts live in the assistant message and the usage record. |
| `TurnStarted` | Live signal | Runtime itself writes the 0006 `TurnStarted { turn_id }` | |
| `SteeringApplied { count }` | Live | Steering messages themselves become `Message` records through `commit_parts` | The event only reports "how many messages applied". |
| `ModelRequestStarted { model_call, budget }` | Live (typical R2) | None | Can drop/merge/rate-limit. |
| `ModelRetrying { attempt, max, delay, reason }` | Live | None | Text of the failed attempt never enters the journal. |
| `ModelTextDelta { text }` | Live (typical R2) | None; the durable form is `content` of the `interrupted` assistant message | Deltas themselves never reach disk. |
| `ModelReasoningDelta { text }` | Live (typical R2) | None; the durable form is the `reasoning` field on the committed message | Can drop/merge/rate-limit. |
| `ToolProgress { index, progress }` | Live (typical R2) | None | Can drop/merge/rate-limit. |
| `HookFailed { hook, message }` | Live (diagnostic) | None; the hook failure fact enters the result message through a synthetic `error_code` | |
| `CompactionStarted` / `CompactionFinished` / `CompactionFailed` / `CompactionSkipped` | Live | Durable counterparts are `ContextCompacted` (candidate) and the replacement commit | Four independent variants, for display. |
| `CompactionUsage { revision, model, usage }` | Delivery notice for a durable fact | Maps to the `CompactionUsage` record | Once per summary call, after the record. |
| `RunFinished { status }` | **Mandatory end state** | No record; the runtime writes the turn end state as `TurnFinished` | The only event-layer variant that must **never drop** (K3). |

### K3: `EventSink` contract

`EventSink::emit` carries the existing comment as normative text: **"Receives events synchronously from the turn loop. Must be short and non-blocking; forward to a queue if the host needs to do more. A bounded queue may drop progress events but must keep `RunFinished`"**. It expands into three rules:

1. **Synchronous, short, non-blocking**: Core sends events synchronously inside the turn loop and applies no backpressure. A host that does heavy work in emit must forward to its own queue.
2. **A bounded queue can drop progress events**: droppable = the K2 live class plus delivery notices (the records already hold the facts).
3. **`RunFinished` never drops**: it has no durable record (the runtime `TurnFinished` carries the turn end state to disk). Delivery through the event channel itself is the only path, so a full queue drops progress first and never drops it.

### K4: Events that runtime itself writes to JSONL

`SessionCreated`, `InboxEnqueued`/`InputClaimed`/`InputAdmitted`, `TurnStarted`/`TurnFinished`/`TurnFailed`, `ConfigChanged`, `PermissionResolved`, `ApprovalPolicyChanged`, `MemoryInjected`, `SessionForked`, `ProjectTrustChanged`, `UserBash`, `BackgroundJobStarted`/`BackgroundJobSettled`. Audit-type (non-critical) events take no part in state rebuild. `replay`/`repair` ignore them.

`McpServerLifecycle` is not in v1.

### K5: Slow-consumer policy

- **Responsibility**: rate limiting, dropping, and merging are **host-side** duties (`EventSink` implementers, TUI subscription layer). The Core side takes no measurements, keeps no queues, and never slows for slow consumers.
- **Drop boundary**: only the K2 live class plus delivery notices can drop. Content that the user saw on screen but the host UI dropped from display can still exist in the journal. The drop loses display, not facts.
- **Never drop**: at event layer = `RunFinished`; at fact layer = all R1 facts, through the journal append mechanism (an append failure ends with `RunError::Journal` plus `RecoveryRequired`). No "ignore on failure" exception exists.

## Constraints and costs

- "Delivery notice" and "live" drop together, so a host that rebuilds durable facts from events loses data: durable facts must come from JSONL/journal reads.
- The `TranscriptReplaced` asymmetry means the host cannot reconcile with "received events == records".
- Sandbox, permission, and privacy behavior is independent of event class.

## Verification

Event order is covered by the ADR 0001 S5 shared order log, the ADR 0003 `StreamTap`, and the ADR 0002 rule "send `ToolStarted` only after the `Started` append succeeds". A host-side test pins "a bounded queue drops progress but keeps `RunFinished`" (the `crates/radian-tui` flood test).
