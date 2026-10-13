# Mutation checks — ADR 0018 automatic compaction (the pure planning half)

Run in a one-time worktree (`git worktree add --detach /tmp/radian-cplan-mut HEAD`) against
commit `728e2b8`, with its **own `CARGO_TARGET_DIR`**, using `cargo test -p agent-core --test
compaction --no-fail-fast`. Each mutation was restored with `git checkout --` and `git
status --short` was clean before moving on.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| M1 | `plan` accumulates per message instead of per atomic unit, so a cut can land inside a parallel tool call | caught (rc 101) | `plan_cuts_only_at_a_unit_boundary` |
| M2 | `plan` drops an oversized last unit (`NothingToCompact`) instead of keeping it whole | caught (rc 101) | `an_oversized_last_unit_is_still_kept_whole` |
| M3 | `truncate_chars` returns the whole tool result (no 2000-character cut) | caught (rc 101) | `the_summary_request_serializes_text_and_truncates_tool_results`, `truncating_a_tool_result_never_splits_a_character` |
| M4 | `summarize_text` includes an assistant's `reasoning` | caught (rc 101) | `the_summary_request_serializes_text_and_truncates_tool_results` |
| M5 | `keep_target`'s max/min are swapped (`quarter.max(MAX).min(MIN)`) | caught (rc 101) | `keep_target_clamps_the_quarter_of_the_budget` |
| M6 | `compacted_history` skips the `validate_history` call | caught (rc 101) | `compacted_history_refuses_an_invalid_kept_suffix` |
| M7 | `units` makes each tool result its own unit instead of part of its assistant's unit | caught (rc 101) | `an_assistant_and_all_its_results_are_one_unit` |

Every mutation was caught; none was equivalent. The randomized
`plan_and_compaction_are_valid_for_many_histories` (200 seeds) is not a mutation target on
its own — it is the broad validity net under `units`, `plan` and `compacted_history`.
