# Mutation checks — radian-core

New rows use the id `<task id>-M<n>` (for example `TK0034-M1`); see "Mutation records" in `AGENTS.md`.

## Rate-limit headers on `ModelResponseReceived` (ADR 0027 slice ④, 2026-10-17)

Base `c09b614` · branch `dogfood/footer-s4`. Each mutation was applied in place, only the catching
binary was run (`--no-fail-fast`), and the source was restored from a `cp` in `/tmp` and confirmed
with `cmp`.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| LIM-A | the turn drops the headers (`limits: response.limits.clone()` → `limits: None` in the `ModelResponseReceived` emit) | ✅ 1 failed (rc=101) | `tests/usage.rs::limits_land_in_the_model_response_received_event` (the first call's `limits_of(0)` is `None`) |

Command: `timeout 300 cargo test -p radian-core --locked --no-fail-fast --test usage limits_land`.
The no-headers half of the same test (`limits_of(1)` is `None`) passes under the mutation: absence
is the default, so only the carrying half can catch a drop.

## Exec `Result.model_error` and the `UsageLimit` variant (TK0184, 2026-10-26)

Branch `work/TK0184-exec-result-model-error`. Each mutation was applied in place, only the
catching test binary was run, and the source was restored from a `cp` under
`.radian/mut-backup`; the restored tree then passed the full crate run again.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0184-M1 | `UsageLimit` made transient (`is_transient` lists it in the `true` arm) | ✅ 1 failed | `tests/retry.rs::a_usage_limit_error_is_never_retried` (the turn sent four requests instead of one and announced a retry) |

## Rework 2: the kind list is declared once (TK0184, 2026-10-27)

Branch `work/TK0184-exec-result-model-error`. `model_error_kinds!` generates `ModelErrorKind`,
`as_str()` and `MODEL_ERROR_KINDS` together. The mutation renamed one declared wire name in that
one declaration; the source was restored from `.radian/mut-backup/model.rs` (`cmp` identical).

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0184-M5 | rename one declared kind (`UsageLimit => "usageLimit"`) | ✅ 2 failed | `model::tests::every_model_error_kind_is_in_the_closed_list` (the variant's pinned name differs, and `usageLimit` is not snake_case) and `bash docs/schema/check-fixtures.sh` (`exec-result.jsonl` line 3: `model_error.kind 'usage_limit'` is not in the closed set parsed from the same declaration) |

## Max tokens continuation (TK0208, 2026-10-27)

Branch `work/TK0208-max-tokens-continuation`. Each mutation was applied in place, only the
catching test binaries were run (`--no-fail-fast`), and the source was restored from
`.radian/turn.rs.clean` (`cmp` identical).

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0208-M1 | the streak never resets on an untruncated response (`else { self.run.max_tokens_streak }` instead of `else { 0 }`) | ✅ 1 failed (rc=100) | `tests/turn.rs::an_untruncated_response_resets_the_streak` (the turn ended `Incomplete(MaxTokens)` instead of `Completed` after the untruncated tool-call response no longer reset the streak) |
| TK0208-M2 | the continuation adds no notice (the `commit` is dropped and `Step::Continue` still returns) | ✅ 5 failed (rc=100) | `tests/turn.rs::a_truncated_response_without_calls_continues_with_a_notice` (the second request carries no notice), `tests/turn.rs::three_truncated_responses_without_calls_end_incomplete_with_two_notices` (0 notices instead of 2), `tests/journal.rs::a_truncation_notice_is_journaled_and_replays` (the replay has no notice), `tests/turn.rs::a_truncated_tool_call_response_counts_toward_the_streak`, and `tests/turn.rs::an_untruncated_response_resets_the_streak` |

## Probe status reads the response prefix (TK0214, 2026-10-27)

Branch `work/TK0214-probe-and-remember-protocol`. The mutation was applied in place, the named
test was run, and the source was restored from `.radian/core_model.rs.bak` (`cmp` empty).

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0214-M8 | `http_status_of` searches for `HTTP ` anywhere instead of requiring the prefix | ✅ 1 failed (rc=100) | `model::tests::http_status_is_read_from_the_response_prefix_only` (`the provider said HTTP 400` read as `Some(400)` instead of `None`) |

## A truncation notice only for a model call that can happen (TK0211, 2026-10-27)

Branch `work/TK0211-max-tokens-continuation-followups`. The mutation was applied in place, only
`tests/turn.rs` was run, and the source was restored from `.radian/turn.rs.clean`
(`cmp` identical).

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0211-M1 | the continuation consults the call cap only again (`self.run.cap_end().is_some()` instead of `self.run.pre_call_end()`) | ✅ 1 failed (rc=100) | `tests/turn.rs::a_passed_deadline_beats_a_truncation_continuation` (the deadline that passed after the response still journaled a notice, `left: 1, right: 0`) |

## The exit-4 truncation rule (TK0226, 2026-10-28)

Branch `work/TK0226-cli-manual-exit-4-truncation-rule`. Each mutation was applied in place, only
`tests/turn.rs` was run, and the source was restored from `.radian/turn.rs.clean` (`cmp`
identical).

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0226-M1 | TK0208's continuation is reverted: no streak is kept and a `MaxTokens` response with no tool calls ends the turn at once | ✅ 2 failed (rc=100) | `tests/turn.rs::two_truncated_responses_without_calls_then_a_normal_one_complete` (the first truncation ended `Incomplete(MaxTokens)` instead of `Completed`) and `tests/turn.rs::truncated_tool_call_responses_share_the_streak` (2 model calls instead of 4: the second truncation ended the turn) |
| TK0226-M2 | a truncated response with tool calls does not count toward the streak (`&& tool_calls.is_empty()` in the streak update) | ✅ 2 failed (rc=100) | `tests/turn.rs::truncated_tool_call_responses_share_the_streak` and `tests/turn.rs::a_truncated_tool_call_response_counts_toward_the_streak` (both ended `Completed` instead of `Incomplete(MaxTokens)`) |

## A small window: the kept part over `K` and the one extra compaction (fix/compaction-small-window, 2026-10-31)

Branch `fix/compaction-small-window`. Each mutation was applied in place, only
`tests/compaction_flow.rs` was run, and the source was restored from a copy taken before the
mutation (`cmp` identical).

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| compaction-small-window-M1 | the kept-part cut never runs (`if false && deterministic_allowed && plan.kept_tokens > keep`) | ✅ 1 failed | `tests/compaction_flow.rs::a_kept_tail_over_the_keep_target_has_its_tool_results_cut` (the compaction came out below target) |
| compaction-small-window-M2 | no extra compaction: a turn with a below-target compaction fails on the next over-budget request (`if true \|\| self.run.extra_compaction_used`) | ✅ 2 failed | `tests/compaction_flow.rs::a_second_compaction_after_a_below_target_one_lets_the_turn_finish` (failed `BelowTarget` instead of `Completed`) and `tests/compaction_flow.rs::a_below_target_extra_compaction_stops_auto_compaction_for_the_turn` (1 compaction instead of 2) |
| compaction-small-window-M3 | the tool-result cut keeps 0 characters (`let limit = 0usize.saturating_sub(reserve)` in `truncate_tool_result_text`) | ✅ 2 failed | `tests/compaction_flow.rs::a_kept_tail_over_the_keep_target_has_its_tool_results_cut` (the head of the result did not survive) and `tests/compaction_flow.rs::the_deterministic_fallback_truncates_tools_and_drops_the_oldest_first` |
| compaction-small-window-M4 | an extra compaction that reached the goal does not clear the below-target state | ✅ 1 failed | `tests/compaction_flow.rs::a_second_compaction_after_a_below_target_one_lets_the_turn_finish` (the third over-budget request failed `BelowTarget` instead of compacting) |
| compaction-small-window-2-M1 | a compaction that reaches the goal does not reset the below-target count (the `else { self.below_target_streak = 0; }` arm emptied in `note_compaction`) | ✅ 1 failed | `tests/compaction_flow.rs::a_compaction_that_reaches_the_goal_allows_another_extra_compaction` (failed `BelowTarget` at the second, non-consecutive below-target compaction); `a_below_target_extra_compaction_stops_auto_compaction_for_the_turn` and `a_second_compaction_after_a_below_target_one_lets_the_turn_finish` still pass |
| compaction-small-window-2-M2 | the over-long fallback never cuts the previous summary (`let kept_summary = match None::<u64> {`) | ✅ 2 failed | `tests/compaction_flow.rs::two_overlong_summaries_cut_the_previous_summary_to_half_the_ceiling` and `tests/compaction_flow.rs::the_summary_does_not_grow_across_two_overlong_fallbacks` |
| compaction-small-window-2-M3 | every fallback cuts the previous summary (`match fallback_too_long.or(summary_budget.map(compaction::summary_max_tokens))`) | ✅ 1 failed | `tests/compaction_flow.rs::a_summarizer_failure_keeps_the_previous_summary_whole` |

## TK0258: the stop reason on ModelUsageRecorded (2026-10-31, branch `work/TK0258-journal-stop-reason`)

Each mutation was applied by hand to `crates/radian-core/src/turn.rs`, backed up under
`.radian/turn.rs.orig` and restored from that copy (`cmp` identical); the named tests ran with
`cargo nextest run -p radian-core --locked --profile sandbox --test usage -E 'test(...)'`.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0258-M1 | the turn loop appends `JournalRecord::ModelUsage` with `stop_reason: None` (the field is omitted) | ✅ 2 failed (rc=100) | `tests/usage.rs::a_truncated_response_with_tool_calls_records_max_tokens_then_end_turn` and `tests/usage.rs::an_other_stop_reason_records_other_and_the_provider_text` |
| TK0258-M2 | the turn loop writes `ModelStopReason::EndTurn` for every response | ✅ 2 failed (rc=100) | the same two tests (`max_tokens` read as `end_turn`; `other` lost its raw text) |
## One consolidated summary instead of a rolling append (TK0259, 2026-10-31)

Branch `work/TK0259-summary-consolidate`. Each mutation was applied in place, only the two
compaction test binaries were run, and the source was restored from `.radian/compaction.rs.clean`
(`cmp` identical).

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0259-M1 | the update paragraph restored to the old "Update the previous summary above with them, preserving what is still relevant" | ✅ 2 failed | `tests/compaction.rs::a_summary_request_with_a_previous_summary_asks_for_one_consolidated_rewrite` and `tests/compaction_flow.rs::a_large_previous_summary_is_consolidated_into_a_small_one` |
| TK0259-M2 | the "newer messages win" sentence removed from the update paragraph | ✅ 2 failed | `tests/compaction.rs::a_summary_request_with_a_previous_summary_asks_for_one_consolidated_rewrite` and `tests/compaction_flow.rs::a_large_previous_summary_is_consolidated_into_a_small_one` |

## Retained reads across a compaction (TK0260, 2026-10-31)

Branch `work/TK0260-compaction-keeps-latest-reads`. Each mutation was applied in place, only
`tests/compaction_flow.rs` was run, and the source was restored from `.radian/compaction.rs.bak`
or `.radian/turn.rs.bak` (`cmp` identical).

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0260-M1 | dedup removed: every read unit becomes a candidate (`if seen.insert(path.clone())` → `if true` in `retained_read_units`) | ✅ 1 failed | `tests/compaction_flow.rs::the_newest_read_of_each_path_is_kept_newest_first` (both reads of each path are kept, so six units and duplicate paths) |
| TK0260-M2 | the budget check removed (`if used.saturating_add(size) > budget` → `if false && …`) | ✅ 2 failed | `tests/compaction_flow.rs::a_budget_for_two_keeps_only_the_two_newest_paths` (the oldest path is kept too) and `a_32k_window_retains_no_read_over_its_budget` (the 3250-token read is retained with a 3072-token budget) |
| TK0260-M3 | the kept-part cut applied to retained units (a `truncate_kept_tool_results` call on `retained_messages` when `plan.kept_tokens > keep`) | ✅ 1 failed | `tests/compaction_flow.rs::a_retained_result_is_not_cut_when_the_kept_tail_is` (the retained 3000-character result comes back cut) |
| TK0260-M4 | the "measure again without" step removed (`if retained_messages.is_empty()` → `if true \|\| …`) | ✅ 1 failed | `tests/compaction_flow.rs::retaining_that_misses_the_goal_measures_again_without` (the committed view keeps the retained unit and is marked `below_target`) |

## A summary floor and a heading (TK0263, 2026-10-31)

Branch `work/TK0263-summary-floor-rejects-non-summaries`. Each mutation was applied in place,
`tests/compaction.rs` and `tests/compaction_flow.rs` were run, and the source was restored from
`.radian/compaction.rs.clean` or `.radian/turn.rs.clean` (`cmp` identical).

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0263-M1 | `SUMMARY_MIN_TOKENS` set to 0 | ✅ 5 failed | `tests/compaction.rs::a_prose_answer_is_too_short` (the prose answer passes the floor and comes back `NoHeading`) and `tests/compaction.rs::the_floor_applies_only_at_ten_times_the_minimum` |
| TK0263-M2 | the heading check removed | ✅ 2 failed | `tests/compaction.rs::a_long_answer_without_a_heading_has_no_heading` (300 tokens of `**Goal**` labels are accepted) and `tests/compaction_flow.rs::two_non_summaries_fall_back_without_too_long` (the second reply is accepted instead of falling back) |
| TK0263-M3 | `TooShort` routed to `shorten_request` instead of the resend | ✅ 2 failed | `tests/compaction_flow.rs::a_prose_answer_resends_the_original_request_and_the_retry_commits` (the second request carries `<summary-to-shorten>`) and `a_short_summary_of_a_long_history_resends_the_original_request` |
| TK0263-M4 | `summary_min_tokens` returns the floor for every history | ✅ 2 failed | `tests/compaction_flow.rs::a_manual_compact_of_a_short_history_accepts_a_short_summary` (the 300-token history's 60-token summary is rejected) and `tests/compaction.rs::the_floor_applies_only_at_ten_times_the_minimum` |
