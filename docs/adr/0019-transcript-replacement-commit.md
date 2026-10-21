# ADR 0019: Transcript Replacement Commit

Status: accepted

## Background

Without a halfway-written replacement segment, recovery must never mistake a "written prefix" for a complete new context. A compacted prefix is an incomplete context. Recovery must distinguish fully committed replacements from incomplete ones, with no new persistence format: append-only JSONL, single writer, and replay stay.

## Decisions

### 1. Two markers

Two new events, both `critical: true`: `TranscriptReplacementCommitted { revision, len }` (commit) and `TranscriptReplacementAborted { revision }` (abort; changes no history, only closes the pending segment). The `replace_transcript` write order is:

```text
TranscriptReplaced { revision, len }
MessageAppended × len          (index 0..len-1)
TranscriptReplacementCommitted { revision, len }
```

The commit event reaches disk successfully first. Only then does the code swap the memory transcript.

### 2. Only a committed replacement takes effect

Read side and Core `replay` and `repair` follow one rule:

- On `TranscriptReplaced`: open a **pending segment**. Earlier history stays (never cleared).
- Inside the pending segment accept only `MessageAppended` records with continuous index from 0.
- On matching `TranscriptReplacementCommitted` (same `revision`, segment count == `len` == event `len`): replace history with the pending segment, clear old-revision tool markers, close the segment.
- On matching `TranscriptReplacementAborted { revision }`: drop the whole segment. History returns to pre-replacement. Later events interpret per pre-replacement history (the next ordinary `MessageAppended` index follows old history).
- **File ends inside a pending segment**: uncommitted, drop the whole segment. Resume writes `Aborted` first.
- **After a pending segment only these may appear**: continuous-index `MessageAppended` inside the segment, matching `Committed`, matching `Aborted`. Any other event (including a new `TranscriptReplaced`) inside a pending segment is **corrupt**. The reader refuses to open and reports the line number.
- A commit or abort that mismatches the segment (different `revision`, count not equal `len`, event `len` not equal segment `len`, no pending segment) is **corrupt**. The reader refuses to open and reports the line number.
- The reader reports each uncommitted segment on `Session` as diagnostics: `revision`, declared `len`, actual written count, position, and end mode (`aborted` / `end_of_file`).

Why `Aborted` is needed: "another event appeared" alone cannot tell where an uncommitted segment ends. Old history length 2, `TranscriptReplaced{len:4}`, then index 0 and 1 written, then crash. After resume, a normally appended message index is exactly 2. It is indistinguishable from "segment item 3". The boundary must live on disk, never only in recovery memory.

### 3. Crash and failure outcomes

| Interrupt point | File state | Recovery result |
|---|---|---|
| Before `TranscriptReplaced` | No replacement | Old history |
| Segment partly written | Pending segment, file ends | **Old history** (segment dropped); resume writes `Aborted` first |
| Segment complete, before commit | Pending segment, file ends | **Old history**; resume writes `Aborted` first |
| After commit, before memory swap | Committed | New history (same as after memory swap) |
| Any journal write fails (process alive) | Same row as above | Live session enters `RecoveryRequired`; memory still holds old history, matching the file recovery result |

So the journal never leads memory by a whole segment: uncommitted means the file recovery result is old history, same as memory.

### 4. Who writes aborts

- **Writer `resume`**: when a scan finds the file ending in a pending segment, it writes `TranscriptReplacementAborted { revision }` and fsyncs before it appends any other event. This is the only required write point.
- **Core `replace_transcript` on in-process failure**: the session enters `RecoveryRequired` and writes nothing more. The abort comes from host resume when it reopens the session. Core never backfills `Aborted` on error paths.
- After `Aborted`, a new `TranscriptReplaced` can open again (for example compaction retry).

### 5. Abort writes are idempotent; read-only opens never write

- `resume` writes `Aborted` only when **the file ends in a pending segment**. After the write the segment is closed. Another crash and another `resume` never repeat the write.
- An `Aborted` line half-written at crash is a torn tail (ADR 0006 §2.2). Truncation per rules leaves the file ending in a pending segment again. The next `resume` writes once more.
- I/O failure while writing `Aborted`: `resume` returns an error and appends nothing else. The session is unwritable. The host reports and exits.
- **Read-only paths never modify the file**: `Session::open`, export, search, `--list`, and index rebuild meet a file ending in a pending segment, interpret it in memory as "uncommitted (`end_of_file`)", report diagnostics, and **never** write `Aborted`. Only writer `resume` writes.
- On corrupt files `resume` refuses. It never writes `Aborted` and never attempts repair.

## Compatibility

Schema stays v1. `critical: true` forces readers that do not know the event to refuse, never to misread per old rules. Fixtures and tests with replacements all carry commit events. An uncommitted segment reads as uncommitted.

## Verification

- `radian-core/tests/replace.rs`: pending segment at file end recovers old history, aborted segment recovers old history with normal append after, committed segment recovers new history, commit or second marker or mismatched commit without segment are replay errors, failed commit append keeps memory and requires recovery, index resets after replacement with one event only.
- `radian-session/tests/replace.rs`: normal growth after commit, commit again after abort, resume writes abort without repeat, torn abort truncates and rewrites, four corruption classes, uncommitted segments stay out of export and index, three consecutive commits fold to the last.
- `docs/schema/check-fixtures.sh` checks all replacement fixtures through an independent state machine. Its conclusions agree with Rust.
