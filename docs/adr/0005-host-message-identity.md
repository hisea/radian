# ADR 0005: Host Message Identity and Transcript Replacement

Status: accepted

## Background

Core keeps the session transcript, but Core is not the authority for inbox, scheduler, MCP, or subagent identity. The host carries its own message marks and origin with the transcript, journal, and events. The host also replaces existing history with host-edited, host-synced, or host-recovered history without restarting tool processes.

## Decisions

### G1: Metadata is host-opaque data

Each `Message` variant carries a `MessageMeta`:

```rust
MessageMeta {
    id: Option<MessageId>,
    source: Option<InputSource>,
    origin: Option<OpaqueJson>,
}
```

All three fields are optional. Only the host generates and interprets `MessageId(String)`. Core only carries it. `origin` keeps the `OpaqueJson` bytes unchanged. Core does not parse or normalize it. Exceptions: Core writes and reads the compaction summary message `origin` (`kind = "compaction_summary"`, including the verbatim `user_messages`) and the memory-addition message with `kind = "memory_addition"`. A host-written `origin` still passes through unparsed.

### G2: The source has no default value

`InputSource` is `#[non_exhaustive]` and serializes in snake_case: `User`, `SlashCommand`, `PermissionDecision`, `Scheduler`, `Mcp`, `Subagent`, `System`. It has no default implementation. `None` means the host provides no source. `None` never equals `User`. A non-human producer therefore never gains human-input semantics or permissions through a Core default. Core also makes no permission decision from it.

### G3: Metadata lives at the message layer, not in the tool result payload layer

The `User`, `Assistant`, and `Tool` `Message` variants each carry `meta`. An empty `MessageMeta` does not serialize. Old JSON without `meta` reads as empty. Each non-empty field is omitted independently. `ToolResult` is the provider-visible result payload and cannot carry host identity. `Message::user`, `Message::assistant`, and every Core-generated message set an explicit empty meta.

### G4: The provider cannot see it

`ModelRequest.messages` still uses `Vec<Message>`, but the provider adapter reads only model-protocol content, tool calls, and tool results. It ignores `meta`. The same history with full meta and with no meta produces byte-identical request bodies.

### G5: Steering stays compatible

`SteeringHandle::steer(impl Into<Message>)` keeps its signature and its message behavior unchanged. A new method `steer_with(message, meta)` accepts only user messages and places the given meta on that message.

### G6: Replacement API, event parameter, and borrow invariant

```rust
async fn replace_transcript(
    &mut self,
    history: Vec<Message>,
    revision: u64,
    events: &dyn EventSink,
) -> Result<(), ReplaceError>
```

`&mut self` already makes a coexisting `ActiveRun` impossible, so no separate "is running" state check exists. `events` is a borrowed sink. This convention follows the existing Core event API convention (`run_turn`, `step`, `run`, and `commit` all take a borrowed sink as a parameter).

### G7: Preconditions, failures, and revision

Replacement runs in this order:

1. A poisoned, closed, or `recovery_required` session returns `ReplaceError::Unusable`.
2. Reuse of `preflight::validate_history` runs next. A failure returns `ReplaceError::InvalidHistory` and writes no journal.
3. The replacement marker, all new messages, and the commit marker are written first (ADR 0019). Only after all writes succeed does Core swap memory as a whole and send events.

Any journal append failure returns `ReplaceError::Journal`, changes no memory, and sets `RecoveryRequired`. The host fully defines `revision`. Core records it but never checks monotonicity or other version relations.

### G8: Journal replacement boundaries

The journal first writes `TranscriptReplaced { revision, len }`, then writes `Message { index, message }` one by one with new-message indices counted from zero, and last writes `TranscriptReplacementCommitted { revision, len }` (ADR 0019). Markers do not repeat the history. Replay opens a pending segment at a marker and replaces history with it only on a matching commit. `repair` also drops the old-revision Started/Settled state, so `recover` applies ADR 0002 rules only to calls still unanswered after the replacement.

### G9: The host owns revision

Core does not compare revisions and does not infer the host version space. The host itself guarantees the order and conflict rules needed by its sync, edit, or recovery flows.

### G10: `ToolSession` does not restart

`replace_transcript` touches only the transcript and the journal. It never closes or reopens the `ToolSession`. The MCP fixture test confirms through the `pid` tool that the process is the same before and after replacement, and through an in-process `counter` that the count continues from 1 to 2.

### G11: Events and journal are intentionally asymmetric

A successful replacement sends only one `AgentEvent::TranscriptReplaced { revision, len }`. The journal still keeps each replacement `Message`, but it sends no `TranscriptAppended` per message: a large history replacement would otherwise flood the host event queue. The asymmetry is intentional. It does not mean the messages lack durable journal records.

## Constraints and costs

- Old and new history occupy memory together during replacement. The journal can lead memory by a whole segment before commit. Recovery drops an uncommitted replacement as a whole and returns to the pre-replacement history (ADR 0019). File and memory therefore reach the same recovery result.
- Core guarantees durability, recovery, and event semantics for a complete replacement. The host still owns file format, revision conflict resolution, identity generation, and any permission policy.

## Verification

`crates/radian-core/tests/replace.rs` covers successful replacement, index reset after markers, "old-revision markers must not repair new history", and `RecoveryRequired` after a journal failure. The `host_message_meta_is_not_sent_to_the_provider` test in `agent-provider-*` pins provider invisibility of meta. `crates/radian-session/tests/replace.rs` covers committed, uncommitted, and aborted segment reads.
