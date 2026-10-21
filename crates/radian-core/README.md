# radian-core

The runtime itself: one turn loop over an **injected** model, tool provider and hooks, with a
write-ahead journal seam. `Agent<M, P, H>` holds the three injections; `AgentSession` is one
conversation (transcript + open tool session); `ActiveRun` is one user turn, driven with
`step()` or `run()`.

It owns: the turn loop, the message/event/hook/steering types, the journal seam
(`TranscriptJournal` + `replay`/`repair`/`recover`), and the context-budget projection.

It does **not** own: files, JSONL, SQLite or any storage format (that is the journal
implementation's job, e.g. `radian-session`); provider wire formats (the two adapters);
permission decisions (hooks decide, `radian-permissions` implements them); sandboxes; and any
notion of a host, a terminal or a session directory. There is no context compression here:
ADR 0015 only *stops* before an oversized request.

## The turn loop

| Public type | Role |
|---|---|
| `Agent<M, P, H>` | The injections. `Agent::new(model, tools)`, `with_hooks`, `with_config`, `config`. |
| `AgentConfig` | `system_prompt`, `max_model_calls` (default `DEFAULT_MAX_MODEL_CALLS` = 32), `model_settings`, `context_budget: Option<ContextBudget>` (`None` disables the projection outright — no budget is ever guessed). |
| `AgentSession` | `open_session` / `open_session_with_history` / `open_session_with_journal`. `start_turn`/`start_turn_with`, `run_turn`, `transcript`, `replace_transcript`, `history_for_resume`, `close`, `is_closed`, `is_poisoned`, `context_budget_status`, `restore_budget_baseline`. |
| `ActiveRun<'_, M, P, H>` | `step`, `run`, `cancellation_handle`, `steering_handle`, `outcome`, `model_calls`, `transcript`. Borrows the session, so one session runs one turn at a time. |
| `TurnOptions` | `deadline`, `steer_mode`, `max_parallel_tools` (default `DEFAULT_MAX_PARALLEL_TOOLS` = 4), `max_model_calls`, `tool_cancel_grace` (default `DEFAULT_TOOL_CANCEL_GRACE` = 5 s). |
| `Step` / `RunOutcome` / `RunStatus` / `RunError` / `SessionError` / `ReplaceError` | Terminal states. `RunStatus` is `#[non_exhaustive]`. |

Model, tools and hooks are traits, not types: `Model` (`ModelRequest` → `ModelResponse`, plus
`ModelContext`/`ModelError`/`ModelUsage`/`StopReason`), `ToolProvider`/`ToolSession`
(`ToolSpec`, `ToolConcurrency`, `ToolContext`, `ToolOutput`, `ToolFailure`, `ToolError`, and
`NoTools` for the empty case), and `Hooks` (`HookContext`, `HookPoint`, `HookError`,
`ToolDecision`). `Model`, `Hooks` and `ToolProvider` are implemented for `Arc<T>`; `Hooks` is
also implemented for `()` and for `(A, B)`.

`ModelError` has three variants: `Transport` (the provider's own trouble — 5xx/408/429 and
connection failures), `Protocol` (the request's — other non-2xx and unreadable streams), and
`ContextOverflow` (ADR 0018: a provider error an adapter recognized as "the input is over the
context window"). All three end the turn `failed` today (ADR 0017); the automatic compaction
that would retry an overflow is a later slice. A host that only has the error text recognizes
the last by `CONTEXT_OVERFLOW_PREFIX`.

`ModelUsage` carries the provider's own numbers unmodified (ADR 0007 J1), so it also records
**what those numbers mean**: `input_includes_cache` is `Some(true)` when `input` already
contains the cache hit (OpenAI-compatible `prompt_tokens`), `Some(false)` when it does not
(Anthropic `input_tokens`), and `None` on a record written before ADR 0007. A
consumer converts instead of adding fields up by hand: `prompt_total()` is the whole prompt
(what the context budget is about) and `uncached_input()` is the part billed at the full input
rate (what the fee is about); `has_billable_counters()` says whether there is anything to
price at all. This is the ADR 0007 change to the public type.

`compaction` (ADR 0018) is the I/O-free half of automatic compaction: `keep_target`,
`units` (the atomic cut points), `plan`, `summary_request`, `check_summary` and
`compacted_history`. It is pure and `async`-free; the turn loop that calls a summarizer,
writes the journal and retries is the `compact-core` slice.

Observations go out through `EventSink` (`AgentEvent`, `NoopSink`); `AgentEvent` is
`#[non_exhaustive]`. Events never change control flow — hooks do.

## The journal seam

`TranscriptJournal::append` receives **every** transcript change before it reaches memory. Inside
the crate, one private helper is the only place that performs the sequence "journal, then memory,
then event", so no other path can touch a mounted session's transcript. `JournalRecord` is
`#[non_exhaustive]`: `Message`, `TranscriptReplaced`, `ToolExecutionStarted`,
`ToolExecutionSettled`, `ModelUsage`. The crate ships the three pure functions that read a
journal back — `replay`, `repair` (the three-rule fill for unpaired calls) and `recover` —
so no host reimplements them.

## What it guarantees (each with the test that holds it)

| Guarantee | Test | File |
|---|---|---|
| Journal first, then memory, then the event; a failed append changes neither memory nor events | `events_mirror_the_journal_in_order`, `journal_failure_sweep` | `tests/journal.rs` |
| An assistant-append failure dispatches **no** tools; a result-append failure runs the tool once and starts no more | `an_assistant_append_failure_dispatches_no_tools`, `a_result_append_failure_runs_the_tool_once_and_starts_no_more` | `tests/journal.rs` |
| An append in flight is never cancelled by cancellation or a deadline | `cancellation_waits_for_the_inflight_append`, `cancellation_waits_for_the_inflight_settled_append`, `the_prefix_append_is_not_cancelled_by_the_cancellation` | `tests/journal.rs`, `tests/markers.rs`, `tests/prefix.rs` |
| The opening history is not re-journaled | `history_is_not_rejournaled` | `tests/journal.rs` |
| A tool call's marker is durable **before** the tool may run | `execution_starts_only_after_started_is_durable`, `tool_started_events_follow_the_durable_marker` | `tests/markers.rs` |
| `ToolExecutionSettled` precedes and matches the result message; a dropped step writes no `ToolExecutionSettled` | `settled_precedes_and_matches_the_result_message`, `dropping_mid_tool_leaves_no_settled` | `tests/markers.rs` |
| Every prefix of a real journal recovers into a valid history | `crash_traversal_recovers_every_prefix` | `tests/markers.rs` |
| `repair` fills unpaired calls by exactly the three rules, and leaves a complete history alone | `repair_fills_unpaired_calls_by_the_three_rules`, `repair_fills_from_settled_exactly_and_leaves_complete_history_alone` | `tests/markers.rs` |
| Cancelling mid-stream commits the prefix the core observed, and the next turn carries it | `cancelling_mid_stream_commits_the_emitted_prefix`, `the_next_turn_carries_the_interrupted_prefix`, `no_deltas_means_no_prefix` | `tests/prefix.rs` |
| Reasoning and visible text are accumulated separately, per model call | `cancelling_during_reasoning_only_commits_the_reasoning_prefix`, `cancelling_mid_stream_keeps_text_and_reasoning_separate`, `each_model_call_accumulates_reasoning_from_scratch` | `tests/prefix.rs` |
| Dropping a **polled** `step()` future poisons the turn and leaves a resumable history; dropping an unpolled one does nothing | `dropping_polled_step_poisons_run`, `dropping_unpolled_step_has_no_effect`, `poisoned_session_history_is_resumable` | `tests/cancel.rs` |
| A cancelled tool gets its grace period, then its future is dropped | `cancelled_tool_gets_grace_to_clean_up`, `tool_ignoring_cancellation_is_dropped_after_grace` | `tests/cancel.rs` |
| `Shared` calls overlap and still commit in call order; `Exclusive` is a barrier; concurrency is bounded | `shared_calls_overlap_and_commit_in_order`, `exclusive_call_is_a_barrier`, `max_parallel_tools_bounds_concurrency` | `tests/parallel.rs` |
| Steering lands at a safe point, and a turn that ends early hands unapplied steering back instead of losing it | `steer_during_batch_injects_before_next_model_call`, `steer_before_first_step_joins_first_request`, `steer_skip_remaining_skips_unstarted_calls`, `unapplied_steering_is_returned_on_cancel`, `steering_without_budget_is_returned_not_lost` | `tests/steering.rs` |
| Only user messages may be steering | `steering_must_be_a_user_message` | `tests/steering.rs` |
| Hook errors fail closed (model hook fails the turn, tool hook denies or replaces the result), a `Rewrite` is revalidated without re-hooking, and a hook cannot change `result.call_id` | `hook_errors_fail_closed`, `rewritten_args_are_revalidated_without_rehooking`, `after_tool_call_cannot_change_call_id` | `tests/hooks.rs` |
| A waiting hook (approval) is raced against cancellation | `approval_hook_races_cancellation` | `tests/hooks.rs` |
| Truncated / filtered / over-budget tool calls are never executed, and each gets a model-readable result | `truncated_tool_calls_are_never_executed`, `content_filtered_tool_calls_are_never_executed`, `max_calls_tool_request_is_not_executed` | `tests/turn.rs` |
| `PauseTurn` continues the same turn with the same budget and keeps opaque blocks | `pause_turn_continues_with_same_budget_and_keeps_opaque_blocks` | `tests/turn.rs` |
| A history with an unpaired tool call is refused at open time | `history_with_unpaired_tool_call_is_rejected` | `tests/turn.rs` |
| Tool schemas with an external `$ref` fail instead of doing I/O | `schema_external_refs_are_disabled` | `tests/turn.rs` |
| Crossing the input budget `B` still sends the request and does not change the transcript | `reported_usage_over_the_budget_still_sends_the_request`, `over_the_budget_does_not_change_the_transcript` | `tests/budget.rs` |
| A projection says whether it is reported, reported+estimated, or estimated; no budget configured means disabled | `without_reported_usage_the_estimate_is_labelled_and_the_request_still_goes`, `no_budget_configured_is_disabled` | `tests/budget.rs` |
| The estimator never rounds down: ASCII bytes/4 rounded up, and at least one token per non-ASCII character | `ascii_code_estimates_at_least_bytes_over_four`, `chinese_text_estimates_at_least_one_token_per_character` | `tests/budget.rs` |
| A turn that crosses `B` ends `Completed`, not stopped | `a_turn_over_the_budget_completes_normally` | `tests/budget.rs` |
| A restored baseline projects the same as an uninterrupted run, and a prefix outside the transcript leaves the estimate alone | `a_restored_baseline_matches_an_uninterrupted_projection`, `a_prefix_outside_the_transcript_leaves_the_estimate_alone` | `tests/budget.rs` |
| Compaction cuts only at atomic units (a user message, or an assistant with all of its results) and keeps an oversized last unit whole | `plan_cuts_only_at_a_unit_boundary`, `an_oversized_last_unit_is_still_kept_whole`, `an_assistant_and_all_its_results_are_one_unit` | `tests/compaction.rs` |
| A summary request flattens text, cuts each tool result to 2000 characters and leaves reasoning out | `the_summary_request_serializes_text_and_truncates_tool_results`, `truncating_a_tool_result_never_splits_a_character` | `tests/compaction.rs` |
| A compacted history is the summary (System source, `compaction_summary` origin) plus the kept suffix, and must pass `validate_history` | `compacted_history_refuses_an_invalid_kept_suffix`, `plan_and_compaction_are_valid_for_many_histories` | `tests/compaction.rs` |
| A transcript replacement is journaled from index zero, emits one event, and keeps the session usable | `replacing_the_transcript_journals_from_zero_emits_one_event_and_continues` | `tests/replace.rs` |
| An invalid replacement is rejected before the journal; a failed replacement keeps memory and requires recovery; unusable sessions reject without journaling | `invalid_replacements_are_rejected_before_the_journal`, `a_failed_replacement_marker_keeps_memory_and_requires_recovery`, `unusable_sessions_reject_replacement_without_journaling` | `tests/replace.rs` |
| Markers from before a replacement never repair a reused call id | `markers_from_before_a_replacement_never_repair_a_reused_call_id` | `tests/replace.rs` |
| Usage follows its assistant message, is written only when present, and a failed append fails the turn | `usage_is_journaled_after_the_assistant_message_and_only_when_present`, `usage_append_failure_sweep` | `tests/usage.rs` |
| `replay`/`recover` ignore usage records; an all-empty `ModelUsage` serializes as `{}`; nested `details` round-trip verbatim | `replay_and_recover_ignore_usage_records`, `default_usage_serializes_as_an_empty_object`, `details_round_trip_preserves_the_nested_object_verbatim` | `tests/usage.rs` |
| Whether `input` contains the cache hit is the adapter's declaration, and `prompt_total()`/`uncached_input()` are the only consumer-side conversions (ADR 0007) | `the_prompt_total_is_the_whole_input_and_the_uncached_part_is_billed_at_the_full_rate`, `an_unmarked_record_is_taken_at_face_value_and_a_bad_one_never_wraps` | `tests/usage.rs` |
| A summarization call's usage is reported once per **attempt** (the controlled retry is a second real call), and not at all when the response reported none (ADR 0018) | `each_summarization_attempt_reports_its_usage_once`, `a_summarization_call_without_usage_reports_nothing` | `tests/compaction_flow.rs` |
| A failed or dropped `close()` can be retried and still closes | `failed_close_can_be_retried`, `dropped_close_can_be_retried` | `tests/close.rs` |

The mutation records for this crate are `MUTATIONS-R1.md` (reasoning, 10/10 caught),
`MUTATIONS-0015.md` (budget estimation), `MUTATIONS-0007.md` (usage) and `MUTATIONS-0018.md`
(compaction planning). A mutation that stays green
is reported as a hole, not hidden: `MUTATIONS.md` in `radian-session` records one (no `fsync`)
that no test in this environment can catch.

## Known limits and what is not verified

- **No context compression in the turn loop (yet).** ADR 0018 replaced ADR 0015's stop: the
  core computes the projection against the input budget `B`, records it, and sends the request
  anyway; it changes nothing in the transcript. The **pure planning half** is in `compaction`
  (`keep_target`/`units`/`plan`/`summary_request`/`check_summary`/`compacted_history`); the
  loop that calls a summarizer and commits the result is the `compact-core` slice. `ContextBudget::input_budget_tokens` is a host-supplied number,
  because the core deliberately has no model→window table. The estimator is a ruler, not an
  audited count: the report says exactly which part is estimated.
- **`HookContext` carries only `cancellation` and `model_calls`** — no run, turn or session
  identity (ADR 0004's recorded gap). A shared `Hooks` instance serving several turns therefore
  cannot tell them apart; `radian-permissions` documents the workaround it needs.
- **Durability is the journal's property, not this crate's.** The core awaits `append` and never
  cancels it; whether the bytes survive a power loss depends on the implementation.
- **Poisoning is by design.** Dropping a polled `step()` future marks the session poisoned and
  fills unpaired calls with `interrupted` results; the host continues with
  `history_for_resume`. After a journal failure (`SessionError::RecoveryRequired`) the returned
  history may end in calls without results and must be repaired (`repair`) before it can open a
  session.
- `AgentEvent`, `JournalRecord` and `RunStatus` are `#[non_exhaustive]`: match with a wildcard.
- The `testing` feature exposes the journal doubles (`RecordingJournal`, `FailingJournal`,
  `SlowJournal`, `SequenceLog`); `journal-selfcheck` additionally makes every session verify at
  close/drop that a recording journal replays to the live transcript.
- Not covered here: Windows-specific behaviour, real provider endpoints, and anything about
  files — all of that lives in the crates around this one.

## Related ADRs

0001 (transcript journal seam) · 0002 (tool execution markers and recovery) · 0003 (interrupted
streaming prefix) · 0004 (hot config slots — records the missing run identity) · 0005 (host
message identity and `replace_transcript`) · 0006 (session JSONL v1 — the format this crate's
`JournalRecord` maps onto) · 0007 (usage) · 0010 (durable vs live events) · 0014 (reasoning
content) · 0015 (context budget estimation) · 0018
(computed input budget `B = min(I, C − R)`).

## ADR 0026 D7: per-turn context budget

`TurnOptions.context_budget: Option<ContextBudget>` overrides
`AgentConfig.context_budget` for one turn, and `AgentSession::compact_now` takes the same
override. This is an additive public-API change. Reason: a `/model` switch (ADR 0026 D5)
must move the context budget `B` with the model, and `AgentConfig` is frozen at `Agent`
construction — the driver has no other way to hand the next turn the new `B`. A `None`
override keeps the configured budget, so no existing caller changes behaviour.
