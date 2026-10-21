# ADR 0006: Session JSONL v1 Format

Status: accepted

## 1. Goals and non-goals

One session owns one JSONL file, and field names form the compatibility promise. This ADR is the unambiguous specification of the disk format: envelope, events, reader handling of unknown content, and the mapping to Core journal records.

This ADR does not define SQLite table schemas (it only requires "JSONL is authoritative, indexes are rebuildable"). It does not define the registry or profile store file layout. It does not cover OAuth, TUI, or sandbox conclusions.

## 2. File and envelope

The host decides the path (at present `<sessions root>/<session_id>.jsonl`). **Single writer**: at most one writer owns one session file at a time.

Each line holds one JSON object:

| Field | Type | Required | Rule |
|---|---|---|---|
| `schema_version` | integer | yes | This ADR defines v1 with value `1`. All lines in one file must carry the same value. A different value means corruption. |
| `session_id` | string | yes | Session identifier, opaque. UUID v4 is recommended. All lines in one file must carry the same value. |
| `seq` | integer | yes | Starts at **0**, steps by **1**, strictly increases, never goes back, never skips (writes serialize per D4; a skip means corruption). `seq` is the line number. It is an independent counter from `MessageAppended.index`. |
| `ts` | string | yes | RFC 3339. The **writer uses UTC with a `Z` suffix** and microsecond precision is recommended. The reader accepts any legal RFC 3339 offset and normalizes it to UTC. The reader must **not** derive order from `ts`. |
| `event` | string | yes | Event name, PascalCase, from the v1 set in §3. |
| `critical` | boolean | yes | Whether a reader that does not know the event skips it safely or loses state reconstruction or safety when it skips it. |
| `data` | object | yes | Event body. |

The envelope allows no field outside this table. The reader ignores an extra field as "unknown optional field", but the writer must not rely on this behavior to store private fields.

### 2.1 Line encoding

- UTF-8. Each line ends with `\n` (LF). A line holds no raw newline (JSON strings escape it). The last line also requires LF. **LF is the commit mark**.
- The writer outputs fixed field order (`schema_version, session_id, seq, ts, event, critical, data`).
- A single line has no hard length cap. The host is recommended to avoid lines above 16 MiB. The reader parses line by line in streaming mode with a configurable line cap (default 64 MiB recommended). A line above the cap means "corrupt", not OOM.

### 2.2 Trailing half line (a crash midway through a write)

The reader acts as follows:

1. An empty file is legal (0 events).
2. A file **not ending in LF** holds a torn tail: the bytes after the last LF are ignored (**record "discarded N bytes"**). They do not count as `seq` and do not enter replay. Even a tail that happens to be legal JSON is still discarded. No LF means no commit.
3. A file ending in LF but holding a line that fails JSON parsing is **corrupt**: refuse to open it (or refuse to append to it). The error names the line and column numbers.

Safe append (the write-back side, including the ADR 0002 E9 repair write-back):

1. First **truncate** the file after the last LF (drop the torn tail).
2. Then append new lines from that offset. Flush after append. Fsync follows the ADR 0040 sync points, not every line.
3. Append without truncation pastes a new line after a half line. This behavior is forbidden.

## 3. Event table and critical

`critical` means: **for a reader that does not know that event type, does skipping it break state reconstruction or safety**. True → an unknown reader must refuse to open the whole file. False → skip the line and record the skip. For a reader that knows all v1 events, the flag is only metadata.

| Event | Critical | Producer | Meaning |
|---|---|---|---|
| `SessionCreated` | true | runtime | Session creation, with reserved fields (§4). It must be line 0. |
| `InboxEnqueued` | true | runtime | Input enqueue (durable inbox). |
| `InputClaimed` | true | runtime | Driver claims a batch of inputs and opens a turn. |
| `InputAdmitted` | true | runtime | Input enters the transcript (reconciles with `MessageAppended.index`). |
| `TurnStarted` | true | runtime | Turn start. |
| `MessageAppended` | true | Core (`JournalRecord::Message`) | One transcript message appended (including synthetic results). |
| `ToolExecutionStarted` | true | Core | Call dispatched. |
| `ToolExecutionSettled` | true | Core | Call settled, with final result. |
| `TranscriptReplaced` | true | Core | Open a pending segment (§5.10). |
| `TranscriptReplacementCommitted` | true | Core | Commit the pending segment. |
| `TranscriptReplacementAborted` | true | Core/writer | Drop the pending segment. |
| `TurnFinished` | true | runtime | Turn end state. |
| `TurnFailed` | false | runtime | Error text for a model or hook error that ends the turn. Audit use. |
| `ConfigChanged` | true | runtime | Configuration effective reference. |
| `Rewound` | true | no current producer | The reader knows the name as a compatible event and keeps the body as opaque `EventData::Other`. It has no typed body contract and no producer or writer path. |
| `ContextBudgetReached` | false | runtime (typed compatible) | Typed audit record. A caller-provided `RuntimeEvent::ContextBudgetReached` can serialize through `append_runtime_event`. The current driver and host have no production path. |
| `ContextCompacted` | false | Core | Compaction candidate record (§5.9). |
| `CompactionUsageRecorded` | false | Core | Usage of the summary call (§5.8). |
| `ModelUsageRecorded` | false | Core | Usage of one dialogue model call (§5.7). |
| `PermissionResolved` | false | runtime | Approval audit. |
| `ApprovalPolicyChanged` | false | runtime | Auto switch audit. |
| `MemoryInjected` | false | runtime | Injected memory set audit. |
| `SessionForked` | false | runtime | Parent-log fork lineage audit. |
| `ProjectTrustChanged` | false | runtime | Project trust write audit. |
| `UserBash` | false | runtime | `!`/`!!` execution audit. |
| `BackgroundJobStarted` | false | runtime | Background task start. |
| `BackgroundJobSettled` | false | runtime | Background task end. |
| `GoalSet` | false | runtime | Goal set or replaced, with goal text and the three-part budget. State source for the goal fold. |
| `GoalBudgetSet` | false | runtime | Goal budget triple replaced as a whole. |
| `GoalProgress` | false | model (`goal` tool) | One progress item. It replaces "latest progress". |
| `GoalDone` | false | model (`goal` tool) | Goal complete (with optional completion note). End state. |
| `GoalPaused` | false | runtime | Goal paused (command, cancellation, or model error). |
| `GoalResumed` | false | runtime | Goal resumed. |
| `GoalDropped` | false | runtime | Goal dropped. The fold forgets it. End state. |
| `GoalWait` | false | model (`goal` tool) | Delay in seconds before the next continuation run. The delay itself stays in-process. |
| `GoalBudgetExhausted` | false | runtime | Budget reached (`turns`, `duration`, or `tokens`). End state. |

Naming note: `Message` becomes `MessageAppended` on disk to avoid confusion with `data.message`. `ToolExecutionStarted/Settled` keep their names. `McpServerLifecycle` is not in v1: MCP child process state never restores across processes.

## 4. `SessionCreated` and reserved optional fields

Required `data` fields: `workspace` (absolute path that the session attaches to) and `config_snapshot` (effective session-level configuration snapshot at creation, holding at least the model route reference).

Optional fields (the reader ignores them when the writer omits them): `agent_id`, `profile_ref`, `workspace_binding`, `parent`.

`parent = { session_id, seq, turn_id?, call_id?, relation? }`: `relation` defaults to `"fork"`. A subagent child session writes `"subagent"`. `seq` is the fork point in the parent log. `turn_id`/`call_id` are optional and record "forked from a turn/call" lineage.

## 5. Event bodies (data)

Convention: the `message` field is the Core `Message` serde JSON unchanged (outer tagging, snake_case). `provider_payload` and `ContentPart::Opaque.raw` are `OpaqueJson` and keep their bytes unchanged. Plain `Value` fields (`ToolCall.arguments`, `annotations`, `meta`, `structured`) can reorder keys lexicographically through a serde_json round trip.

### 5.1 `MessageAppended`

| Field | Type | Required | Meaning |
|---|---|---|---|
| `index` | integer | yes | Equals the transcript length at append time. Replay requires continuity from 0. |
| `message` | `Message` JSON | yes | An assistant variant can carry `interrupted` (false does not serialize) and `reasoning` (empty does not serialize). All three variants can carry `meta`. The `meta` inner field order is fixed as `id, source, origin`. A fully empty `meta` key never appears. |

A compaction summary message is a `User` message with `source = System` and `origin = {"kind":"compaction_summary","user_messages":[…]}`. A memory-addition message is a `User` message with `source = System` and `origin = {"kind":"memory_addition","records":[…]}`.

### 5.2 `ToolExecutionStarted` / `ToolExecutionSettled`

`Started`: `call_id`, `name`, `arguments` (post-Rewrite parameters actually passed to the tool). `Settled`: `call_id`, `result` (post-`after_tool_call` final result; synthetic or recovery causes live in `result.error_code`; no independent `synthetic` field).

### 5.3 `InboxEnqueued` / `InputClaimed` / `InputAdmitted`

| Event | Fields |
|---|---|
| `InboxEnqueued` | `message`; `message_id` (at least 1 character, opaque); `lane`: `"next_turn" \| "next_step"`; `source` (`user \| slash_command \| permission_decision \| scheduler \| mcp \| subagent \| system`); `wake` (boolean, optional; default derives from lane: `next_turn` true, `next_step` false); `degraded` (boolean; degraded-gate injection; default `false`); `pending_wake` (boolean; at turn end a stranded entry still opens a turn; default `false`, ADR 0011 L5). |
| `InputClaimed` | `message_ids` (non-empty string array); `turn_id`. |
| `InputAdmitted` | `message_id`; `transcript_index`. |

Identities and recovery:

1. The transcript message for `InputAdmitted.message_id` must carry `message.meta.id == message_id`.
2. Runtime copies `InboxEnqueued.source` into `meta.source` when it writes the message. When both exist they must agree.
3. Recovery: an input with `InputAdmitted` never replays. An input with `InputClaimed` but no matching `InputAdmitted`, and with no transcript message carrying that `meta.id`, returns to the head of its original lane (it is not lost and not duplicated; it never executed, because "enter the transcript" precedes tool side effects). `InboxEnqueued.message_id` is the dedup key. **The transcript is the authority for "already admitted"**: when a `meta.id` already appears in a `MessageAppended` message, the reader fills the missing `InputAdmitted` and never replays.

`wake` records whether this input opens a new turn by itself when the driver is idle: `steer` true, `inject` false. The writer always writes it explicitly. A reader that meets a record without the field derives it from the lane.

`degraded` records whether this input is a degraded-gate injection (budget exhausted: inject but do not wake). It appears only when true (past records stay byte-identical). A reader that meets a record without the field reads false.

`pending_wake` records whether that `next_step` input opens a new turn by itself when it still waits in the queue at turn end (the stranded work-completion-notice case, ADR 0011 L5). It appears only when true. A reader that meets a record without the field reads false.

### 5.4 `TurnStarted` / `TurnFinished` / `TurnFailed`

- `TurnStarted`: `turn_id`.
- `TurnFinished`: `turn_id`; `outcome` (Core `RunStatus` in snake_case); `reason` (optional, see below). `outcome` values: `completed`, `refused`, `budget_exceeded`, `failed`, `poisoned`, `incomplete`, `cancelled`. The reader also accepts the historical value `context_budget`. `TurnOutcome::ContextBudget` is a typed compatible shape. A caller-provided `TurnFinished { outcome: ContextBudget }` can serialize. The current driver and host never send it. `incomplete` requires `reason` with value `max_tokens` / `content_filtered` / `tool_use_without_calls` / `unknown_stop_reason` (the last one carries a `stop_reason` string). `cancelled` requires `reason` with value `requested` / `deadline_exceeded`.
- `TurnFailed`: `turn_id`, `message` (adapter-masked error text). Non-critical audit.

| Core `RunStatus` | Disk `outcome` | `reason` |
|---|---|---|
| `Completed` | `completed` | — |
| `Refused` | `refused` | — |
| `BudgetExceeded` | `budget_exceeded` | — |
| `Failed` | `failed` | — |
| `Poisoned` | `poisoned` | — |
| `Incomplete(MaxTokens)` | `incomplete` | `max_tokens` |
| `Incomplete(ContentFiltered)` | `incomplete` | `content_filtered` |
| `Incomplete(ToolUseWithoutCalls)` | `incomplete` | `tool_use_without_calls` |
| `Incomplete(UnknownStopReason(s))` | `incomplete` | `unknown_stop_reason` (plus `stop_reason`) |
| `Cancelled(Requested)` | `cancelled` | `requested` |
| `Cancelled(DeadlineExceeded)` | `cancelled` | `deadline_exceeded` |

History compatibility: the reader accepts `outcome = "context_budget"` (`TurnOutcome::ContextBudget`). It is a serializable typed compatible shape. The current driver and host never send it.

### 5.5 `ConfigChanged`

| Field | Type | Required | Meaning |
|---|---|---|---|
| `revision` | integer | yes | Monotonic per-session configuration revision number (from 1). |
| `change` | object | yes | Effective content, for example `{"model":{"provider":"…","model":"…"}}`, `{"thinking":"high"}`, `{"approval":"auto"}`. Host labels only. It holds **no secret**. |
| `profile_ref` | object | no | From profile application: `{ id, revision, snapshot_hash }`. |

### 5.6 `PermissionResolved` / `ApprovalPolicyChanged`

- `PermissionResolved`: `request_id`, `call_id`, `decision` (`allow\|deny\|rewrite`), `approver` (`user\|policy`), `reason` (optional), `policy` (optional; names the external policy command when `approver = "policy"`; no current producer). Approvals **never replay across resume**.
- `ApprovalPolicyChanged`: `policy` uses serde `snake_case`: `on_ask`, `never`, `always`, `auto`. The durable effective state is the `ConfigChanged { "approval": … }` written at the same point. `ApprovalPolicyChanged` is the permission audit event.

### 5.7 `ModelUsageRecorded`

| Field | Type | Required | Meaning |
|---|---|---|---|
| `usage` | object | yes | `ModelUsage` serde shape. All seven subfields are optional and absent by default: `input`, `output`, `total`, `cache_read`, `cache_write`, `details` (`OpaqueJson`), `input_includes_cache` (boolean; default = unmarked). Empty usage is `{}`. "Not reported" is a missing field, not 0. Core never computes `total`. |
| `fee` | object | no | `{ "micros": integer, "incomplete": boolean? }`. ADR 0027 call fee. An old record without it restores through the current price table and marks the value as estimated. |

Order: directly after the assistant message it describes, before tool dispatch. When the response carries no usage, no record is written. `replay`/`repair`/`recover` ignore it.

### 5.8 `CompactionUsageRecorded`

`revision`, `model`, `usage`, `fee` (optional). True usage of one summary call. It counts as cost whether or not the compaction commits. It is not `ModelUsageRecorded`. Resume never uses it as a budget baseline.

### 5.9 `ContextCompacted`

`revision`, `source_revision`, `source_len`, `summary`, `first_kept`, `tokens_before`, `tokens_after`, `below_target`, `model`, `deterministic` (optional, default false; true for the deterministic fallback). It is only a candidate record: compaction takes effect only after the `TranscriptReplacementCommitted` with the same `revision` reaches the disk.

### 5.9a Compatible shapes: `Rewound` / `ContextBudgetReached`

The two compatible names differ:

- `Rewound`: the reader knows the name and treats it as a critical compatible event. It keeps the body as opaque `EventData::Other` unchanged. It has no typed body contract, no producer path, and no writer path. It defines no fields.
- `ContextBudgetReached`: the typed compatible shape in `runtime.rs` (`turn_id`, `source`, `projected_tokens`, `window_tokens`, `threshold_percent`, optional `baseline_input_tokens`/`estimated_growth_tokens`). `append_runtime_event` can serialize a caller-provided record. The current driver and host have no production path. Non-critical audit.

### 5.10 Replacement: `TranscriptReplaced` / `Committed` / `Aborted`

`TranscriptReplaced { revision, len }` opens a pending segment. Later `MessageAppended` records count indices from 0 again. `TranscriptReplacementCommitted { revision, len }` commits: it replaces history with the pending segment and clears old-revision tool markers. `TranscriptReplacementAborted { revision }` drops the pending segment.

Replay rules live in one Core place (`TranscriptFold`), per ADR 0019. Uncommitted segments are never exported and never indexed.

### 5.11 `SessionForked`

`child_session_id`, `parent_seq`. Written in the parent log. Non-critical audit. The child session `SessionCreated.parent` is the lineage authority.

### 5.12 `MemoryInjected` / `ProjectTrustChanged` / `UserBash`

- `MemoryInjected { records: [{ id, revision, scope }] }`: records only id/revision/scope. It never copies content.
- `ProjectTrustChanged { path, decision }`: `path` is the normalized directory. `decision` is boolean.
- `UserBash { command, exclude_from_context, exit_code, cancelled }`: `!`/`!!` audit. `!!` records the same way.

### 5.13 `BackgroundJobStarted` / `BackgroundJobSettled`

- `BackgroundJobStarted { job_id, command, output }`: `command` is the first-line summary. `output` is the `job` prompt (the output file lives in the session directory, and the model cannot read it).
- `BackgroundJobSettled { job_id, state, duration_ms }`: `state` uses the `[status: …]` spelling (`exited 0`, `signal 9`, `killed`, `timed out`).

## 6. Unknown content policy

(a) **Unknown optional fields** (in the envelope and inside known event `data`, including nested message structures): **ignore** them. The reader can log them in a skipped log, but it must not fail.

(b) **Unknown event type**: follow the envelope `critical` flag. False → skip the line and record the skip. True → **refuse to open the whole file**. The error names the file, line number, event name, and supported schema version.

(c) **`schema_version` above reader support**: the host chooses read-only open or direct refusal. It must **never continue to write with old-version semantics**.

(d) **Unknown enum value in a known field**: map it to that enum `Unknown` in memory. No error and no skipped line. V1 on-disk enum list: `InboxLane`, `TurnOutcome`, `TurnReason`, `PermissionDecision`, `PermissionApprover`, `InputSource`. A new value for an existing enum does not raise `schema_version`, but the writer must first confirm that an old reader still behaves correctly when it reads the value as `Unknown`. `Unknown` lives only in memory and never writes back in normalized form.

## 7. Forward and backward samples

- Backward: `fixtures/v1-minimal.jsonl`. Any v1 reader accepts it in full.
- Forward: `fixtures/v1-forward.jsonl`. `SessionCreated` carries all reserved optional fields, plus one future non-critical event (`HeartbeatRecorded`). The reader opens it normally, skips that event, and rebuilds the rest in full.
- Independent fixtures exist for each unknown-content type and for torn tails. `fixtures/README.md` states the expectation per file. `check-fixtures.sh` `EXPECT` is the script authority.

## 8. JSONL is authoritative, SQLite is only an index

After SQLite deletion, JSONL alone must fully rebuild: index fields all derive from envelope and file; derived tables replay from the event stream; the index stores no "fact that lives only in the index". Strict `seq` increments plus the LF-commit and truncation rules form the precondition for index consistency.

## 9. Sensitive information

**Never in JSONL**: API keys, Authorization/Cookie request headers, secret reference values (only the reference ID is allowed), environment variable values, `.netrc` or keychain content.

**Allowed but treated as sensitive**: `provider_payload` and `Opaque` hold provider-returned bytes verbatim and can contain model-echoed sensitive content. Treat the JSONL file as potentially sensitive data (permissions, backups, cross-provider boundaries).

## 10. Write paths

- `SessionJournal::create()` writes line 0 (content from the caller: `workspace`, `config_snapshot`).
- `TranscriptJournal::append` writes all nine current `JournalRecord` variants (`Message`, `TranscriptReplaced`, `TranscriptReplacementCommitted`, `TranscriptReplacementAborted`, `ToolExecutionStarted`, `ToolExecutionSettled`, `ModelUsage`, `CompactionUsage`, `ContextCompacted`; see ADR 0001 D2) and keeps `MessageAppended.index` continuous.
- `append_runtime_event` writes typed `RuntimeEvent` values. The event type decides `critical`; the caller does not pass it. This path never writes `MessageAppended`/`TranscriptReplaced`/tool markers/usage.
- Both paths share one write lock and one `next_seq`. Each line is selected, rendered, written, and flushed while the lock is held. Only `must_sync` events (the ADR 0040 sync points) fsync again. The counter advances only after a successful write. The event body serializes into `Box<RawValue>` outside the lock. `OpaqueJson` enters the file byte by byte.
- Runtime events never enter `journal_records()`. `replay`/`repair`/`recover` ignore them.

## 11. Verification

Read and write tests in `crates/radian-session` cover envelope completeness, `seq` continuity, LF endings, `OpaqueJson` round trips, accept/skip/reject/torn-tail expectations for all fixtures, safe append after torn tails, `index` continuity, over-new schema versions, crash injection, index rebuild, and repair write-back. `docs/schema/check-fixtures.sh` checks all fixtures through an independent Python state machine. Its conclusions agree with Rust.
