# ADR 0001: Transcript Journal Seam

Status: accepted

## Background

A durable session cannot rely on a comparison of the transcript after each step. A crash during a tool batch leaves an unauditable gap: memory holds results, side effects have occurred, and no record reconciles the two. Core therefore awaits a host-provided journal write before each change to the in-memory transcript. Core changes memory and sends events only after the write succeeds. Core does not know files, JSONL, or SQLite. Core knows only one trait.

"Success" means the record is written and flushed. ADR 0040 defines which record is truly durable: records before externally visible side effects and records at boundaries reach the disk first.

## Decisions

### D1: Dyn-compatible trait, mounted when the session opens

`TranscriptJournal` is a dyn-compatible trait (`append(&self, JournalRecord) -> BoxFuture<'static, Result<(), JournalError>>`). The host mounts it through `Arc<dyn TranscriptJournal>` in `Agent::open_session_with_journal(history, journal)`. Core does **not** add generic parameters to `Agent`/`AgentSession`/`ActiveRun`: the journal is per-session wiring, not a type-level property. Generics would spread to every caller that names these types.

### D2: `JournalRecord` is a `#[non_exhaustive]` enum

Nine variants exist at present:

- `Message { index: usize, message: Message }`
- `TranscriptReplaced { revision: u64, len: usize }`
- `TranscriptReplacementCommitted { revision: u64, len: usize }`
- `TranscriptReplacementAborted { revision: u64 }`
- `ToolExecutionStarted { call_id: String, name: String, arguments: Value }`
- `ToolExecutionSettled { call_id: String, result: ToolResult }`
- `ModelUsage { usage: ModelUsage }`
- `CompactionUsage { revision: u64, model: String, usage: ModelUsage }`
- `ContextCompacted { revision: u64, source_revision: u64, source_len: usize, summary: String, first_kept: usize, tokens_before: u64, tokens_after: u64, below_target: bool, model: String, deterministic: bool }`

External matches require a wildcard arm.

### D3: Core never cancels an in-flight append

Cancellation and an expired deadline **wait for the current append** to complete before they converge. The journal implementation owns timeout behavior and returns `Err` on timeout. A dropped write future leaves an unknown state ("the write may or may not have landed"). For a WAL, that state is worse than slowness. The `SlowJournal` plus virtual-time test proves that cancellation waits for the in-flight append.

### D4: Appends from one session are strictly serial, and index equals the memory length at write time

The caller awaits each change in `commit` before it makes the next change. The writes are therefore serial by construction. `index` is `transcript.len()`, so journal replay rebuilds the transcript in order.

### D5: The journal leads memory by at most one record, and never lags memory

"Write the journal first, then change memory" means a drop or crash between the two steps leaves the journal ahead (by at most one record). Recovery trusts the journal. Memory must never lead the journal (except for the D7 drop repair). "Write" here is `write` plus `flush`. Records before a sync point are lost together with memory on a **system crash** (a process crash does not lose them). The inequality therefore holds under both crash types. ADR 0040 defines the truly durable point.

### D6: Any append failure ends the turn with `RunError::Journal`, and the session enters `RecoveryRequired`

Memory and journal must not continue after they diverge. A failed append changes no memory and sends no event. The turn ends with `RunStatus::Failed` plus `RunError::Journal`. Later `start_turn*` calls return `SessionError::RecoveryRequired`. The host must **rebuild the session from the journal**: `recover(records)` replays messages and pairs open calls with results through the markers (ADR 0002 / E8). The host must **not** reopen directly with `history_for_resume().history`: it can contain unpaired tool calls, and `open_session_with_history` rejects such history. Only `unapplied_steering` from that history remains directly usable. `close()` remains usable. A batch that fails midway stops dispatching further calls. Memory keeps the journaled prefix. The transcript can contain unpaired tool calls. This state is **intentional**: the recovery side (the `repair` function in ADR 0002) decides pairing. Core does not invent results midway.

### D7: The synchronous drop path does not touch the journal

A dropped future poisons in `Drop`, and `Drop` cannot await. The replacement `interrupted` result **enters memory only**. The session still ends as `Poisoned`. At that point memory leads the journal, and the leading part holds only tool results with `error_code == "interrupted"` (`journal-selfcheck` asserts this property). The durable repair happens when the session reopens.

### D8: `history` does not re-enter the journal

History passed at open time is already durable. The first record uses `index == history.len()`.

### D9: `start_turn` stays synchronous, and user input commits at the first `step()`

The synchronous `start_turn_with` cannot await the journal, so the input stays in `ActiveRun` as `pending_input`. It commits at the start of the first `step()` in the turn (before the cancellation check, so even an immediately cancelled turn keeps "input is in the transcript"). Before the first `step()`, `transcript()` does **not** show the input. A turn dropped before its first `step()` never commits the input. That state equals "the turn never started".

### D10: Feature `testing` and `journal-selfcheck`

`testing` exports `RecordingJournal` / `FailingJournal` / `SlowJournal` and `SequenceLog` (a shared log for cross-cut assertions on order). `journal-selfcheck` (which implies `testing`) mounts an internal `RecordingJournal` automatically for each session **without an explicit host journal**. On successful `close()` and on `Drop`, it asserts: for a normal session, journal replay equals the transcript message by message; for a poisoned session, the journal is a prefix of the transcript and the extra tail holds only `interrupted` tool results. On a thread that already panics, it skips the assertion (to avoid a double-panic abort). A session with a host journal is not readable back, so it skips automatic checks. Its own tests assert the behavior.

## Semantics

- **S1 Order**: each change = successful `append` → push to memory → send `TranscriptAppended { index, message }`. A failure triggers none of the three steps. The only implementation is `agent::commit_parts` (`AgentSession::commit` and the batch context share it).
- **S2 Full coverage**: user input, steering injection, assistant messages, tool results (batch prefix commits), synthetic results from `resolve_unexecuted` (truncated / content filtered / budget exhausted), preflight failures / rejections / hook errors / cancellations / skipped steering — all pass through `commit_parts`. No bypass exists (`journal-selfcheck` verifies the full suite).
- **S3 One-by-one commit for multi-message sequences**: when message j fails, the first j−1 messages are already journaled and in memory. The rest do not happen. `memory == journal`.
- **S4 Dispatch gate**: an assistant message with tool calls dispatches no tool before its journal write succeeds (`commit` runs before `dispatch_tools`). The following `ToolExecutionStarted` sync point makes the preceding assistant message (with tool calls) durable together with it: the sync point fsyncs first, then the tool runs.
- **S5 Events**: `TranscriptAppended` indices are consecutive, match the journal one to one, and are sent only after each append returns (the shared order log verifies this property).

## Constraints and costs

- While Core awaits a journal commit inside a `tokio::select!` branch, it does **not** poll other completed parallel tools. The append must therefore be fast. This behavior is the cost of serializing journal writes with tool completion interleavings. Tests prove that batches stay complete, ordered, and deadlock-free.
- After a mounted journal, a batch that fails midway leaves a transcript that can hold unpaired tool calls (D6). That state is auditable input to the ADR 0002 repair. It is better than an invented result.

## Verification

The named tests in `crates/radian-core/tests/journal.rs` cover D1–D10 and S1–S5. `cargo test -p radian-core --features journal-selfcheck` runs the full replay assertion suite. `cargo tree` shows no new dependencies.
