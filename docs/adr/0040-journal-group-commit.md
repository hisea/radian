# ADR 0040: Journal Group Commit

Status: accepted

## Background

Per-record fsync spends almost a whole tool round trip on syncing. Two crashes need separate views: **process crashes** (panic, `kill -9`, terminal close) leave post-`write` data in OS page cache, still landing on disk, fsync-independent. Only **system crashes** (power loss, kernel crash) lose un-fsynced data. ADR 0002 recovery rules already cover "tool started, no end record". Recovery synthesizes `outcome_unknown_after_crash`. The model learns that call outcome is unknown.

## Decisions

1. **Writes stay per-record, order unchanged, format unchanged**. Only fsync timing changes.
2. **Mandatory fsync points** (all records before them land with them):
   - Session create (first row).
   - Before any outer side effect. Before tool dispatch: after `ToolExecutionStarted`, before real tool run. Background `shell` starts count as tool calls. Before host-run side effects of its own.
   - Turn end (`TurnFinished`/`TurnFailed`).
   - Replacement commit (`TranscriptReplacementCommitted`) and writer-`resume` `TranscriptReplacementAborted`.
   - Session close.
3. All other records (model reply commits, usage, tool ends, config changes, compaction candidates, and similar) only `write`. The next fsync point carries them.
4. **Invariant**: any outer side effect happens only with its "doing this" record already on disk. The worst post-system-crash result: tool results completed after the last fsync point lose, and recovery shows those calls as `outcome_unknown_after_crash`. "Side effects happened with no journal record" never occurs.
5. Rejected: fsync only at turn end. It leaves tool-changed files on disk with no matching journal records.

Implementation: `radian-session/src/writer.rs` `must_sync(event)` picks sync points by disk event name. One row picks `seq` under lock, renders, writes, flushes, fsyncs when needed. Counters advance only after successful writes.

## Acceptance

- Per-step fsync counts drop from a dozen-plus to 2–3. The perf baseline doc records new single-step and tool round-trip times.
- Fault injection: fake system crashes between each two fsync points (drop bytes written after the last fsync). Post-recovery transcripts agree, unfinished tools all read `outcome_unknown_after_crash`, no run side effect lacks `ToolExecutionStarted`. `journal-selfcheck` fully passes.
- Mutations: no fsync before tool dispatch (tests must find "side effects happened with records off disk"). No fsync at turn end. No fsync at replacement commit.
