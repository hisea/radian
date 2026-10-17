# radian-session

Session JSONL v1 storage (ADR 0006): the durable `agent_core::TranscriptJournal` on disk, the
streaming reader and replay path, the **disposable** SQLite index, and the export renderers.

It owns the file format and its forward-compatibility policy. It does **not** own: the turn
loop or the transcript's meaning (that is `agent-core`, whose own `replay`/`repair`/`recover`
this crate calls rather than reimplements), the inbox/driver (`radian-runtime`), permissions,
or the shape of the config file (ADR 0008).

## Write side

`SessionJournal` is the `TranscriptJournal` implementation. `create(path, session_id, created)`
takes the exclusive lock, truncates any torn tail and writes `SessionCreated` at seq 0;
`resume(path, session_id)` opens an existing file, truncates the torn tail (ADR 0006 §3.2),
verifies `schema_version`/`session_id` **from the file**, and continues `seq` after the last
complete line. `append_runtime_event(RuntimeEvent)` writes a runtime-owned event through the
same lock, `seq` counter and sync-point decision (ADR 0040); `next_seq()`, `session_id()`,
`path()`, and `close()` (the explicit close-time fsync; `Drop` does the same best-effort).

- One writer per file, enforced by a kernel `flock` (`WriteError::Locked`). A contended
  `flock` is retried with backoff for ~200 ms before `Locked` is reported, so the transient
  false conflict caused by a child that inherited the lock in another thread's `[fork, exec)`
  window (released at the child's `exec`) is not mistaken for a second writer.
- The line's `seq` is chosen, rendered and written **while the lock is held** (ADR 0040:
  `fsync`ed there too when the record is a sync point), and the counter only advances once the
  write succeeded — so `seq` order is file order even when the core's journal records and the
  runtime's events are appended concurrently.
- Group commit (ADR 0040): every append is `write` + `flush`; only the sync points
  (`SessionCreated`, `ToolExecutionStarted`, `TurnFinished`, `TurnFailed`,
  `TranscriptReplacementCommitted`/`Aborted`) `fsync`, and close/`Drop`
  `fsync`s the tail. A group of records becomes durable together, so a step fsyncs 2–3 times
  instead of once per record.
- A new file is created owner-only (0600) at creation time, not chmod'ed afterwards.
- `critical` is a property of the event type, never a caller argument.

`RuntimeEvent` is the typed runtime write path: `InboxEnqueued`, `InputClaimed`,
`InputAdmitted`, `TurnStarted`, `TurnFinished`, `ConfigChanged`, `ContextBudgetReached`,
`PermissionResolved`, `MemoryInjected`, plus `RuntimeEvent::name()`, `critical()` and
`permission_resolved()`. `PermissionResolved` and `MemoryInjected` are `critical: false`
(audit); `MemoryInjected` carries only `{ id, revision, scope }` per record, never content
(ADR 0021 D6).
Transcript events are deliberately **absent** from it: they must go through
`TranscriptJournal::append` so `MessageAppended.index` stays contiguous with what `replay`
expects.

## Read side

`Session::open(path)` validates a file per ADR 0006 §7 and retains every accepted event:
`events()`, `skipped()` (the `(seq, event)` of unknown non-critical events),
`recovered_replacements()` (every replacement segment that never reached its declared `len`),
`committed_bytes()`, `schema_version()`, `session_id()`, `path()`. `journal_records()` maps the
events back to core records, and `history()` folds them through `agent_core::recover`, so an
unpaired tool call gets the ADR 0002 synthetic result.

`EventData` is the typed body of one accepted line; `SessionEvent` is the envelope plus the
line's byte offset and length (the index's primary keys); `RecoveredReplacement` is the
diagnostic for a short replacement segment.

The unknown-content policy (ADR 0006 §7) is enforced line by line: unknown optional fields are
ignored; an unknown non-critical event is skipped and reported; an unknown **critical** event
refuses the whole file, naming path, line and event; a higher `schema_version` refuses to be
written; a known field holding an unknown enum value becomes that enum's `Unknown`
(`#[serde(other)]`) rather than a corruption error.

## The index (a disposable cache)

`Index::open(path)` creates the SQLite database owner-only, in WAL mode with a busy timeout
(`journal_mode()`, `busy_timeout()`). `rebuild_dir(dir)` / `rebuild(paths)` re-derive every row
by scanning the files with the **same** crate-private streaming parser the reader uses — so the
LF-commit / torn-tail rule is shared by construction. `list(workspace)` and
`search(query, workspace, limit)` answer from the index alone; `indexed_events(session_id)` gives
the raw rows; `audit(path)` compares the index against the file; `reconcile(path)` rebuilds when
they disagree.
`read_line_at(path, offset, len)` returns the original line a hit points at. `IndexError`,
`SessionSummary`, `IndexedEvent`, `SearchHit`, `RebuildWarning`, `Audit`, `Reconcile`.

Every column except `sessions.path` and `sessions.indexed_at` is re-derivable from the JSONL.
The JSONL is the authority and **no operation here ever writes it**. Search is `LIKE` over the
visible text of user/assistant messages only (tool results, tool-call arguments and provider
payloads are not indexed), with `LIKE` metacharacters escaped.

## Export

`export_json` / `export_markdown` (and the `*_to` file variants) render a session with
non-optional redaction. `ExportOptions` has two per-export opt-ins, both off by default:
`include_provider_payload` (ADR 0013 decision 3) and `include_reasoning` (ADR 0014).
`with_secret` adds an always-redacted string; there is deliberately **no** switch that turns
redaction off. `redact_value` / `redact_text` are public because a host that prints its own
diagnostics must redact the same way.

## Filesystem privacy

`ensure_private_dir` / `ensure_private_file` create with 0700/0600 and only **warn** about an
existing path that is group/world-accessible (`wide_permissions_warning`, `warn_if_wide`,
`PRIVATE_DIR_MODE`, `PRIVATE_FILE_MODE`, `open_private_append`). Changing permissions the user
set on purpose would be a surprise; telling them is not.

## What it guarantees (each with the test that holds it)

| Guarantee | Test | File |
|---|---|---|
| A session file has the v1 envelope and sequential `seq`s from 0 | `a_session_file_has_the_v1_envelope_and_sequential_seqs` | `tests/write.rs` |
| `OpaqueJson` payloads reach the file byte-for-byte | `opaque_json_round_trips_byte_for_byte_through_the_file` | `tests/write.rs` |
| Resuming continues the sequence and keeps the session id | `resuming_continues_the_sequence_and_keeps_the_session_id` | `tests/write.rs` |
| A tool result settles with its full shape | `a_tool_result_settles_with_its_full_shape` | `tests/write.rs` |
| A torn tail is truncated before the next append (and `O_APPEND` means no sparse hole) | `a_torn_tail_is_truncated_before_appending` | `tests/append.rs` |
| A killed writer leaves only a torn tail | `a_killed_writer_leaves_only_a_torn_tail` | `tests/append.rs` |
| A second writer is refused — in this process and in another one | `a_second_writer_in_this_process_is_refused`, `a_second_writer_in_another_process_is_refused` | `tests/append.rs` |
| A lock a forked child inherited is retried until the child releases it; a real second writer is still refused after the budget | `a_lock_inherited_by_a_forked_child_is_retried_and_succeeds`, `a_held_lock_is_still_refused_after_the_retry_budget` | `tests/append.rs` |
| Every partial line length is truncated before the next append | `every_partial_line_is_truncated_before_the_next_append` | `tests/crash_inject.rs` |
| A full turn round-trips through the file and continues; the fold agrees with an independent line scan | `a_full_turn_round_trips_through_the_file_and_continues`, `the_fold_agrees_with_an_independent_line_scan` | `tests/e2e.rs` |
| Repair write-back appends normally and keeps `seq` continuous | `repair_write_back_appends_normally_and_keeps_seq_continuous` | `tests/e2e.rs` |
| An explicitly mounted journal is the only one in play | `an_explicit_journal_is_the_only_one_mounted` | `tests/e2e.rs` |
| Sensitive values never reach the file | `sensitive_values_never_reach_the_file` | `tests/e2e.rs` |
| The v1-minimal fixture is accepted with a contiguous transcript; the forward fixture skips the unknown event and ignores reserved fields | `v1_minimal_is_accepted_with_a_contiguous_transcript`, `v1_forward_skips_the_unknown_event_and_ignores_reserved_fields` | `tests/read_fixtures.rs` |
| An unknown non-critical event is skipped and reported; an unknown critical event refuses the file with its line and event | `an_unknown_noncritical_event_is_skipped_and_reported`, `an_unknown_critical_event_refuses_the_file_with_line_and_event` | `tests/read_fixtures.rs` |
| A newer `schema_version` is refused and never written; a mid-file version change is corruption | `a_newer_schema_version_is_refused_and_never_written`, `a_schema_version_change_mid_file_is_corruption` | `tests/read_fixtures.rs` |
| A torn tail is dropped at read time; a bad JSON line names its line number; a message-index gap is corruption | `a_torn_tail_is_dropped_at_read_time`, `a_bad_json_line_names_its_line_number`, `a_message_index_gap_is_corruption` | `tests/read_fixtures.rs` |
| An unknown on-disk enum value maps to `Unknown` instead of refusing the file | `unknown_enum_values_are_mapped_to_unknown_not_refused`, `an_unknown_enum_value_in_a_runtime_event_maps_to_unknown` | `tests/read_fixtures.rs`, `tests/runtime_events.rs` |
| A crash after `ToolExecutionStarted` repairs the unsettled call | `crash_after_started_repairs_the_unsettled_call` | `tests/read_fixtures.rs` |
| A replacement resets the index and repairs only the new revision | `replace_resets_the_index_and_repairs_the_new_revision_only` | `tests/read_fixtures.rs` |
| Corruption replay is total, and never writes the file | `corruption_replay_is_total_and_never_writes_the_file` | `tests/fuzz.rs` |
| Runtime events keep `seq` continuous without touching the message index, round-trip through typed reader variants, write their documented `critical` flag, and never enter the transcript fold | `runtime_events_keep_seq_continuous_and_do_not_touch_the_message_index`, `runtime_events_round_trip_through_typed_reader_variants`, `every_runtime_event_writes_its_documented_critical_flag`, `runtime_events_never_enter_the_transcript_fold` | `tests/runtime_events.rs` |
| Concurrent runtime and core appends keep `seq` in file order | `concurrent_runtime_and_core_appends_keep_seq_in_file_order` | `tests/runtime_events.rs` |
| The runtime-event write path respects the single-writer lock and truncates a torn tail like every other line | `the_new_method_respects_the_single_writer_lock`, `the_new_method_truncates_a_torn_tail_like_every_other_line` | `tests/runtime_events.rs` |
| `PermissionResolved` round-trips and is not replayed | `permission_resolved_round_trips_and_is_not_replayed` | `tests/permission_event.rs` |
| `MemoryInjected` round-trips without content, is `critical: false`, is never a transcript record, and the exports show it | `memory_injected_round_trips_without_content_and_is_not_critical` | `tests/runtime_events.rs` |
| `MemoryInjected` round-trips without content, is `critical: false`, is never a transcript record, and the exports show it | `memory_injected_round_trips_without_content_and_is_not_critical` | `tests/runtime_events.rs` |
| A normal append after a complete replacement is not counted as part of it; replacements and the appends between them fold to the last revision | `a_normal_append_after_a_complete_replacement_is_not_counted_as_part_of_it`, `replacements_and_the_appends_between_them_fold_to_the_last_revision` | `tests/replace.rs` |
| Resume continues after a short replacement, and after that replacement is replaced again | `resume_continues_after_a_short_replacement`, `resume_continues_after_a_short_replacement_is_replaced_again` | `tests/replace.rs` |
| A crash mid-replacement is recovered as the written prefix; a short replacement before a new marker reads back; an interior empty line refuses resume; an orphan tool marker is refused | `a_crash_mid_replacement_is_recovered_as_the_written_prefix`, `a_short_replacement_followed_by_another_replacement_reads_back`, `resume_refuses_an_interior_empty_line`, `a_marker_whose_call_id_never_appeared_is_refused` | `tests/findings.rs` |
| Every indexed field is derivable from the JSONL; a deleted index rebuilds to the same rows; a stale index is detected and rebuilt from the JSONL | `every_indexed_field_is_derivable_from_the_jsonl`, `a_deleted_index_rebuilds_to_the_same_rows`, `a_stale_index_is_detected_and_rebuilt_from_the_jsonl` | `tests/index.rs` |
| A torn tail is not indexed and is picked up after truncate-and-append | `a_torn_tail_is_not_indexed_and_is_picked_up_after_truncate_and_append` | `tests/index.rs` |
| The index uses WAL and a busy timeout; two readers, and a writer with a reader, share one index without busy failures | `the_index_uses_wal_and_a_busy_timeout`, `two_readers_share_one_index_without_busy_failures`, `a_writer_and_a_reader_share_one_index_without_busy_failures` | `tests/index.rs` |
| Sessions are isolated by workspace; a search hit locates back to the original line; search covers user and assistant text but not tool results; `LIKE` metacharacters are matched literally; list and search answer from the index with the file moved away | `sessions_are_isolated_by_workspace`, `a_search_hit_locates_back_to_the_original_line`, `search_covers_user_and_assistant_text_but_not_tool_results`, `like_metacharacters_in_a_query_are_matched_literally`, `list_and_search_answer_from_the_index_with_the_file_moved_away` | `tests/index.rs` |
| Extra indexed rows are a mismatch even when the counts agree | `extra_indexed_rows_are_a_mismatch_even_when_the_counts_agree` | `tests/index.rs` |
| Both exports omit the provider payload by default and include it when opted in; Markdown follows the same default; reasoning is omitted by default in both formats | `json_export_omits_provider_payload_by_default_and_includes_it_when_opted_in`, `markdown_export_follows_the_same_payload_default`, `reasoning_is_omitted_by_default_in_both_formats_and_included_when_opted_in` | `tests/export.rs` |
| Secrets never appear in any export, and a named secret is redacted even without a known pattern | `secrets_never_appear_in_any_export`, `a_named_secret_is_redacted_even_without_a_known_pattern` | `tests/export.rs` |
| Export never modifies the session file | `export_never_modifies_the_session_file` | `tests/export.rs` |
| Thinking blocks are excluded from the default export | `thinking_blocks_are_excluded_from_the_default_export` | `tests/export.rs` |
| A new session directory and file are owner-only; an existing wide one is reported and **not** changed; the index database and its WAL are owner-only | `a_new_session_directory_is_owner_only`, `a_new_session_file_is_owner_only`, `an_existing_wide_session_directory_is_reported_and_not_changed`, `an_existing_wide_index_is_reported_and_not_changed`, `the_index_database_and_its_wal_are_owner_only` | `tests/privacy.rs` |

Mutation records: `MUTATIONS.md` (writer/reader/crash/fuzz/privacy) and `MUTATIONS-INDEX.md`
(the index and export slice). Both state the misses, not only the catches.

## Known limits and what is not verified

- **`fsync` as a power-loss proof is not provable here**, though its *timing* now is: no test
  in this environment can simulate a power loss, and a killed process still leaves its bytes in
  the page cache, so removing `sync_all()` from a single record leaves the durability tests
  green (recorded in `MUTATIONS.md` as an *equivalent mutation under the test environment*).
  The group-commit tests (`tests/group_commit.rs`, `radian-runtime/tests/group_commit.rs`)
  instead pin *when* the fsync happens — the per-step count and the crash-at-every-record
  traversal fail if a sync point stops syncing. The measured cost it buys is ~4 ms per fsync
  on this machine (ADR 0006 §15.2; the per-step numbers are in `docs/perf-baseline.md`).
- **Space cost, accepted:** `ToolExecutionSettled` carries the full result and the following
  `MessageAppended` carries the same result message again, so a large tool output is stored
  **twice** (ADR 0006 §15.1). v1 does not change this.
- **A session's memory grows with its length.** `Session::open` retains every event; only the
  index path (`scan_stream`) streams without retaining.
- **Line length:** a fixed internal cap (64 MiB, `MAX_LINE_BYTES` in `reader.rs`) refuses an
  oversized line instead of growing the buffer. ADR 0006 §16 still lists the suggested 16/64 MiB
  values as **not performance-tested**.
- **Not in v1:** cross-process locking beyond `flock` is not designed here; `ts` clock skew is
  not normalised (order is `seq` only); GB-scale files are not measured (ADR 0006 §16).
- **`Session::skipped` and `Session::recovered_replacements` are different things** and are
  deliberately kept apart: `skipped` is the ADR 0006 §7b list of unknown events and the index
  derives `skipped_count` from it, so a short replacement is never smuggled in there.
- **A short replacement that later happens to be completed** by a normal append is
  indistinguishable from a complete replacement and is not reported; `replay` only requires
  `index` contiguity, so nothing is lost.
- **The reader never rewrites the file** — including a torn tail, which is only *not counted*.
  `SessionJournal::resume` is the one path that truncates it, and only on the write side.
- Search is `LIKE`, not FTS: no ranking, no stemming, and only `MessageAppended` text is
  searchable.
- `MUTATIONS-ANTHROPIC.md` cites a `radian-session` test named
  `provider_payload_is_written_byte_for_byte`; **no such test exists in this tree**. The
  byte-for-byte guarantee is held by `opaque_json_round_trips_byte_for_byte_through_the_file`
  (and, on the adapter side, by `a_payload_read_back_from_json_still_echoes_thinking` in
  `agent-provider-anthropic`). The reference in that file should be corrected.

## Related ADRs

0001 (the journal seam this implements) · 0002 (markers, and the `repair` rules the fold uses) ·
0005 (message identity, `replace_transcript`, and the short-replacement recovery rule) · 0006
(session JSONL v1 — the spec) · 0007 (usage records) · 0010 (durable vs live) · 0013
(privacy: owner-only modes, redaction that cannot be turned off, no telemetry) · 0014
(reasoning is opt-in in an export) · 0015 (the `ContextBudgetReached` audit event).
