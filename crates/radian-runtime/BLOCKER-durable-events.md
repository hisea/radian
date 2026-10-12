# Blocker: `radian-session` cannot write runtime durable events

**Status:** commit `a81f817` (queue semantics) is complete and green. The durable half of
Phase 2 / A (driver + `InboxEnqueued`/`InputClaimed`/`InputAdmitted`/`TurnStarted`/
`TurnFinished`/`ConfigChanged`) is **blocked**: it needs a public API that `radian-session`
does not have, and this slice must not change `radian-session`.

## What is needed

ADR 0010 K4 and ADR 0011 L3 say the **runtime** writes these JSONL events (producers =
runtime). ADR 0006 §3 requires one file per session with a single writer and a strictly
continuous `seq`. So the runtime must append those events through the same writer that
holds the file lock and the `seq` counter.

## Evidence (as of `a28fa3b`)

`crates/radian-session/src/writer.rs` — the complete public surface of the writer:

```text
SessionJournal::create(path, session_id, SessionCreatedData)
SessionJournal::resume(path, session_id)
SessionJournal::session_id() / path() / next_seq()
impl TranscriptJournal for SessionJournal { fn append(&self, JournalRecord) -> BoxFuture<Result<(), JournalError>> }
```

`render_record`, `render_line`, `write_line_numbered`, the envelope struct, the `seq`
counter and the `flock` are **private**, and `render_record` only maps the five
`agent_core::JournalRecord` variants:

| `JournalRecord` | disk event |
|---|---|
| `Message` | `MessageAppended` |
| `TranscriptReplaced` | `TranscriptReplaced` |
| `ToolExecutionStarted` | `ToolExecutionStarted` |
| `ToolExecutionSettled` | `ToolExecutionSettled` |
| `ModelUsage` | `ModelUsageRecorded` |

There is **no** `JournalRecord` variant and **no** writer method for `InboxEnqueued`,
`InputClaimed`, `InputAdmitted`, `TurnStarted`, `TurnFinished` or `ConfigChanged`.

Reading is not a problem: `Session::open` already recognises those names
(`reader.rs` `KNOWN_EVENTS`) and returns them as `EventData::Other(serde_json::Value)`, so
recovery can parse them without any change.

## Why the obvious workarounds do not work

1. **A second writer on the same file** (open it again and append): breaks ADR 0006 §3's
   single-writer rule; `SessionJournal` already holds the `flock`, and the second writer
   would need its own `seq` counter, producing duplicate `seq` values and interleaved
   lines.
2. **The runtime re-implements the envelope/render/fsync**: duplicates `radian-session`
   and violates "落盘经 `radian-session`"; it also cannot reuse the private `seq`/`flock`.
3. **A sidecar file** (e.g. `<session>.inbox.jsonl`): breaks the one-file format, the
   reader's `seq`/session checks and the SQLite `Index` rebuild.
4. **Pass the events as `JournalRecord`s**: no variant exists, and adding one is a change
   to `agent-core` (also off-limits).

## Proposed minimal API (additive, no behaviour change)

```rust
impl SessionJournal {
    /// Appends one runtime durable event (ADR 0006 §4; producers = runtime), with the same
    /// envelope field order, `seq` continuity, LF termination and fsync as core records.
    ///
    /// `event` must be a runtime event name (`InboxEnqueued`, `InputClaimed`,
    /// `InputAdmitted`, `TurnStarted`, `TurnFinished`, `ConfigChanged`, `PermissionResolved`,
    /// `SessionForked`, `ContextCompacted`, `Rewound`); the five core events are rejected
    /// because `TranscriptJournal::append` owns them.
    pub fn append_event(
        &self,
        event: &str,
        critical: bool,
        data: serde_json::Value,
    ) -> BoxFuture<'static, Result<(), JournalError>>;
}
```

- Uses the existing private `render_line`/`write_line_numbered` and the same `next_seq`
  counter, so ordering and `seq` stay correct with respect to core records.
- Failure semantics identical to `append` (the caller turns it into
  `RunError::Journal` + `RecoveryRequired`, ADR 0001 D6).
- `SessionJournal::next_seq()` already exists, so tests can assert the interleaving.

An alternative the owner may prefer is a typed enum (e.g. `RuntimeEvent::InboxEnqueued {
message, lane, source, message_id }`) with `append_runtime_event(&self, RuntimeEvent)`;
it is more type-safe but a larger surface. Either unblocks the slice.

## What I did not do

- Did not modify `radian-session` or `agent-core`.
- Did not implement the driver, the durable writes, recovery or the end-to-end test; they
  all depend on the API above.
- Queue semantics (commit `a81f817`) are independent of the writer and stand on their own.

## Also still undecided (from ADR 0011)

`followup`/`inject` **whether a `source` is required** is still open (the "仍未决定"
section). This slice follows the core: `source` is optional and the inbox neither requires
nor invents one (there is a test for both directions).
