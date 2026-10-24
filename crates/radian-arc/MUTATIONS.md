# `radian-arc` mutation record (gate slice, ADR 0048)

New rows use the id `<task id>-M<n>` (for example `TK0034-M1`); see "Mutation records" in `AGENTS.md`.

Base: `201f1bc6` for M1–M5; `0352b129` for M6–M7 (the environment-isolation change landed
between them); the fix-round commits for M8–M12. Each mutation was applied to the committed
file, run with `cargo nextest run -p radian-arc --no-fail-fast`, restored from a copy under
`.radian/fix1-backup/`, and the restored file was compared byte for byte (`cmp`) with the
copy.

| # | Mutation | Caught by | Result |
|---|---|---|---|
| M1 | drop the sha filter in `valid_verdicts` and in the digest's counted set | `verdict::a_verdict_for_an_old_sha_does_not_count_after_the_branch_moves`, `verdict::the_digest_changes_when_a_verdict_of_the_same_sha_is_replaced_by_a_new_attempt` | ✅ 2 failed |
| M2 | `clean -ffdx` without `-e target` | `checkout::prepare_removes_a_previous_sha_and_keeps_target` | ✅ failed |
| M3 | `create_new` replaced by `create` + `truncate` (verdict overwrite allowed) | `commands::a_verdict_cannot_be_written_twice` | ✅ failed |
| M4 | `CommandRun::run` does not take the exclusive lock | `commands::a_commands_stage_waits_for_the_exclusive_lock` (the intended assertion); `commands::two_exclusive_stages_in_two_threads_never_overlap` (the two prepares race on one checkout and panic) | ✅ 2 failed |
| M5 | `git rebase <head>` instead of `git merge --no-ff <head>` | `integration::integration_with_main_moved_merges_and_runs_key_commands_on_the_merge` (candidate has one parent, not two); `integration::a_conflict_gives_a_fail_verdict_with_the_path` (the checkout is left inside the rebase) | ✅ 2 failed |
| M6 | drop `-c core.hooksPath=` from the git helper | `checkout::a_core_hooks_path_hook_never_runs` | ✅ failed after the environment fix below |
| M7 | `kill(pid, SIGKILL)` instead of `kill(-pid, SIGKILL)` (only the direct child) | `commands::a_timeout_kills_the_whole_command_group` | ✅ failed |
| M8 | `required_passed` back to "any passing verdict for the sha", ignoring which attempt is newest | `verdict::a_newer_attempt_decides_whether_a_required_stage_passed` | ✅ failed |
| M9 | `fetch_remote` ignores its remote argument and fetches the literal `origin` | `checkout::prepare_fetches_the_named_remote`, `integration::integration_uses_the_named_remote` | ✅ 2 failed |
| M10 | drop the `GIT_DIR`/`GIT_WORK_TREE`/… clearing from the git helper | `checkout::an_inherited_git_dir_does_not_redirect_prepare` | ✅ failed |
| M11a | an empty leftover directory at the checkout path is refused instead of cleared | `checkout::an_empty_leftover_checkout_directory_is_cleared` | ✅ failed |
| M11b | clear any directory at the checkout path, empty or not (`remove_dir_all`, no emptiness check) | `checkout::a_non_empty_checkout_directory_is_refused` | ✅ failed |
| M12 | drop the `gates/integration.lock` from `IntegrationRun::run` | `integration::two_integration_runs_never_overlap` | ✅ failed |
| M13 | `run` starts a `todo.ready` task of an `auto` `off` plan | `run::auto_off_starts_nothing_but_still_verifies` | ✅ failed |
| M14 | `run` merges a plan at `auto` `run` (only `off` is skipped) | `run::one_run_takes_a_chain_to_awaiting_human_and_stops` | ✅ failed |
| M15 | `max_auto_rework` never pauses (`max = u32::MAX`) | `run::max_auto_rework_pauses_the_plan_and_sets_auto_off` | ✅ failed |
| M16 | `run` does not take `runner.lock` | `run::a_second_run_while_one_holds_the_lock_exits_four` | ✅ failed |
| M17 | the wrapper forwards SIGTERM to the worker pid only, not its process group | `run::a_cancelled_attempt_whose_worker_started_sleep_leaves_no_sleep_process` | ✅ failed |
| M18 | build the verification candidate but run the `commands` stage on `verified_sha` | `review::main_that_breaks_a_gate_command_sends_the_task_to_rework`, `review::main_that_a_gate_command_needs_makes_the_task_pass` | ✅ 2 failed |
| M18b | write `main_sha` and `candidate_sha` as `None` in the `commands` verdict | `review::verification_uses_verified_sha_when_main_has_not_moved` | ✅ failed |
| M19 | treat a failed candidate merge as `candidate == verified_sha` (skip the conflict check) | `review::a_verification_merge_conflict_makes_rework_pending_with_the_path` | ✅ failed |
| M20 | count a running gate in `[limits] concurrency` (`start_ready` adds `background::running` tasks to `count_doing`) | `run::a_running_gate_does_not_take_a_concurrency_slot` | ✅ failed |
| M21 | treat any heartbeat as alive (`is_some` instead of `age <= window`) | `attempt::reconcile_judges_a_stale_heartbeat_dead_without_calling_ps`, `attempt::reconcile_retries_a_dead_wrapper_without_a_result` | ✅ 2 failed |
| M22 | ignore the held gate lock in `background::start` (a pass starts a second gate) | `run::a_gate_is_not_started_twice_while_it_runs` | ✅ failed |
| M23 | stop enforcing `max_wall_time_secs` in the wrapper (go back to a monitor in the `run` process) | `run::a_worker_still_stops_at_its_limit_after_the_run_process_is_killed`, `attempt::a_wall_time_signals_the_wrapper_and_records_the_class` | ✅ 2 failed |
| M24 | write `""` as the attempt of a refused gate record | `run::a_refused_gate_is_reported_once_and_not_restarted` | ✅ failed |
| M25 | stop the heartbeat when the worker exits, before the delivery and push checks | `run::a_wrapper_pushing_past_the_stall_window_is_not_judged_dead` | ✅ failed |
| M26 | install no SIGTERM handler in the gate (the stop reaches only the gate pid) | `run::sigterm_to_watch_kills_the_running_gate_stage` | ✅ failed |
| M27 | leave the wrapper's git commands unbounded (the wall clock starts at the model child again) | `run::a_hanging_push_ends_the_attempt_at_the_wall_time` (40 s deadline: `start` never returned) | ✅ failed |
| M28 | leave the inherited gate-lock descriptor inheritable (no `FD_CLOEXEC` reset) | `run::a_stage_child_does_not_keep_the_gate_lock` (the lock stayed held by an orphan `sleep`) | ✅ failed |
| M29 | leave the gate in the loop's process group (drop `process_group(0)` from its spawn) | `run::sigint_to_a_gate_stops_its_stage` (`kill(-<pid>, SIGINT)` fails: the gate has no group of its own) | ✅ failed |
| M30 | drop the no-record guard (the pass's `started` record is left alone and a new gate starts on every pass) | `run::a_gate_that_exits_without_a_record_is_reported_once_and_not_restarted`, `run::a_watch_pass_does_not_restart_a_gate_that_left_no_record`, `run::a_changed_head_starts_a_gate_that_left_no_record_once_more` | ✅ 3 failed |
| M31 | skip the early finish record (`run_inner` returns the failure without `record_failure`) | `run::a_wrapper_that_refuses_its_arguments_frees_the_slot_in_the_next_pass`, `run::a_wrapper_whose_worktree_lock_is_held_frees_the_slot_in_the_next_pass`, `work::a_second_wrapper_for_the_same_task_fails_on_the_flock` | ✅ 3 failed |
| M32 | skip the rework round's `pre_existing` move (record 0 and move nothing) | `review::a_blocking_item_the_rework_did_not_touch_is_moved_and_the_stage_passes` | ✅ failed |
| M33 | treat the whole task's own diff as the increment (drop the intersection with `previous..sha`) | `review::a_rework_prompt_holds_the_increment_and_the_previous_items`, `review::a_blocking_item_the_rework_did_not_touch_is_moved_and_the_stage_passes` | ✅ 2 failed |
| M34 | drop rule (b) of `ReworkRound::keeps` (an earlier unfixed item stops blocking) | `review::a_blocking_item_near_an_unfixed_previous_item_still_blocks` | ✅ failed |
| M35 | write the review rework when the plan's `auto` is `off` | `review::a_failed_review_with_auto_off_waits_for_the_lead` | ✅ failed |
| M36 | treat a review of the commit the previous review reviewed as a rework round (drop the `previous.sha != sha` guard) | `review::a_reverify_of_the_same_head_is_a_first_review_and_still_blocks` | ✅ failed |
| M37 | drop the recorded-pair guard in `review::auto_reverify` | `run::the_same_head_and_main_are_reverified_automatically_once` | ✅ failed |
| M38 | drop the count (`len >= u32::MAX` instead of `len >= max_auto_reverify`) | `run::max_auto_reverify_stops_the_step_and_says_why` | ✅ failed |
| M39 | also reverify a review failure (`stage == "review"` passes the fixable check) | `run::only_a_command_failure_of_an_auto_plan_is_reverified` | ✅ failed |
| M40 | drop the "remote main moved" condition (any failed `commands`/integration stage qualifies) | `run::a_command_failure_is_not_reverified_while_main_stands_still` | ✅ failed |
| M41 | ignore the priority in the claim order (`claim_order` compares the ids only) | `run::a_higher_priority_plan_gets_the_free_slot_first`, `run::a_priority_change_takes_effect_on_the_next_pass_and_does_not_preempt`, `arc::priority_is_set_listed_and_shown_and_a_bad_value_exits_three` (the `list` order) | ✅ 3 failed |
| M42 | let a higher priority preempt a running attempt (`start_ready` ignores the tasks in `doing`) | `run::two_plans_at_the_same_priority_keep_id_order`, `run::a_higher_priority_plan_gets_the_free_slot_first`, `run::a_priority_change_takes_effect_on_the_next_pass_and_does_not_preempt` | ✅ 3 failed |
| M43 | accept `P6` (drop the set check when a plan file is read and in `set_priority`) | `store::model::tests::a_priority_outside_the_set_is_refused_and_the_order_is_priority_then_id`, `store::an_old_plan_without_a_priority_reads_as_p3_and_keeps_its_bytes`, `arc::priority_is_set_listed_and_shown_and_a_bad_value_exits_three` | ✅ 3 failed |
| M41 | let the loop claim a `todo.proposed` task (accept `proposed` in `run::start_ready`'s phase filter and in `attempt_ops::claim`) | `review::the_loop_does_not_claim_a_proposed_task_and_start_refuses_it` | ✅ failed |
| M42 | move the plan back to `draft` on a task confirm | `review::confirm_makes_a_proposed_task_ready_and_leaves_the_plan_alone` | ✅ failed |
| M43 | include the review verdict's `blocking` items in the follow-up task | `review::a_blocking_item_is_never_carried_into_the_followup` | ✅ failed |
| M44 | pass `--resume` with `fresh`: `claim` sets `resume: number > 1` (the mode check dropped) | `attempt::a_fresh_claim_names_each_attempts_session_and_never_resumes`, `attempt::a_failed_outcome_retries_with_the_backoff_then_is_exhausted`, `review::rework_after_reject_starts_a_new_attempt_with_its_own_verdicts` | ✅ 3 failed |
| M45 | drop the hand-off section (a fresh later attempt's stdin is the brief alone) | `work::a_fresh_rework_starts_a_new_session_with_the_hand_off`, `work::a_fresh_rework_keeps_the_conflict_section_first`, `work::a_long_previous_report_is_cut_at_the_cap_with_a_note` | ✅ 3 failed |
| M46 | put the hand-off before the conflict section | `work::a_fresh_rework_keeps_the_conflict_section_first` | ✅ failed |
| M47 | read only attempt `n-1` for the hand-off's report (`newest_report` does not walk back) | `work::the_hand_off_walks_back_past_a_reverify_attempt` | ✅ failed |
| M48 | compare `max_attempts` with the attempt number again (`attempt < max_attempts`) | `attempt::a_reworked_task_keeps_starting_after_max_attempts_worth_of_attempts`, `attempt::retries_after_a_rework_start_from_zero_and_stop_at_max_attempts`, `attempt::a_failed_outcome_retries_with_the_backoff_then_is_exhausted` | ✅ 3 failed |
| M49 | do not reset `retry_count` on a rework (`plan_ops::add_rework_with` drops the reset) | `attempt::a_reworked_task_keeps_starting_after_max_attempts_worth_of_attempts` (round 2 keeps the count at 1) | ✅ failed |
| M50 | drop the conflict limit (`if false && record.reworks >= limit`) | `review::max_conflict_rework_stops_a_run_of_conflicts_and_says_why` (the fourth conflict returns to `todo.ready`), `review::a_zero_conflict_limit_never_writes_the_rework` (the brief is written at `0`), `review::a_conflict_limit_of_one_writes_one_brief_and_then_waits` (a second brief is written at `1`) | ✅ 3 failed |
| M51 | the conflict path consults `max_attempts` again (`task.attempt >= max_attempts \|\| …`) | `review::a_conflict_on_the_old_last_attempt_still_gets_the_conflict_rework` (the third attempt with `max_attempts` 1 stops instead of returning to `todo.ready`) | ✅ failed |
| M52 | drop `retry_count` from the task JSON (`store::cli::task_json`) | `arc::the_retry_count_is_a_field_of_show_and_list_json` | ✅ failed |
| M53 | drop the reset after a passing verification (`finish_verification`) | `review::a_non_conflicting_verification_resets_the_conflict_count` (the second conflict after the pass stops the task) | ✅ failed |
| M54 | drop the reset in `attempt_ops::apply` for an attempt that never reached the gate | `attempt::an_attempt_that_does_not_reach_the_gate_resets_the_conflict_count` (the run stays at 2) | ✅ failed |
| M55 | do not reset the conflict run for the lead's `add_rework` (`plan_ops::add_rework_with` drops the empty-suffix reset) | `review::a_lead_rework_resets_the_conflict_run` (the record keeps `reworks: 3` after the lead's brief) | ✅ failed |
| M56 | the pass restarts a dead `running` verify gate without counting (`advance_verification` drops the `count_death` call) | `run::a_gate_that_dies_after_running_is_restarted_twice_then_left_died`, `run::zero_max_gate_deaths_stops_at_the_first_death` (both run to the 60 s limit, with an `verify started` line on every pass) | ✅ 2 failed |
| M57 | the loop's own stop leaves `running` instead of writing `interrupted` (`background::stop` skips `write_interrupted`) | `run::a_stopped_gate_is_interrupted_and_restarted_without_a_death` (the record says `running`) | ✅ failed |
| M58 | `radian arc reverify` refuses a stuck `review.verifying` task again (`attempt_ops` sets `gate_stuck = false`) | `run::reverify_reruns_a_task_whose_gate_left_no_record`, `run::reverify_reruns_a_task_whose_gate_died` (exit 3, no attempt) | ✅ 2 failed |
| M59 | `background::stop` ignores a gate whose record is still `started` (`STARTED => false`, no lock check) | `run::sigterm_to_watch_stops_a_gate_that_has_not_written_running` (the gate outlives the loop and later writes `done`) | ✅ failed |
| M60 | `background::stop` assumes a gate with no start time is alive (`started.is_empty() \|\| …`); the extra mutation of the brief's item (d) | `run::stop_without_a_start_time_returns_as_soon_as_the_gate_is_gone` (waits its whole grace and then the kill window) | ✅ failed |
| TK0034-M1 | the plan form forgets to set `auto` `off` (both writes in `cancel_plan` dropped) | `attempt::cancelling_a_plan_ends_every_open_task_and_the_plan` | ✅ failed |
| TK0034-M2 | `list` shows a cancelled plan without `--all` (the cancelled-plan skip in `render` dropped) | `list::a_cancelled_plan_is_hidden_until_all_in_text_and_json` | ✅ failed |
| TK0034-M3 | the claim path ignores the plan status (`start_ready` drops its `status != "confirmed"` check and `attempt_ops::claim` drops its plan-status refusal) | `run::a_run_pass_claims_and_gates_nothing_from_a_cancelled_plan` | ✅ failed |
| TK0034-M4 | the pass drops the step that ends a cancelled plan's `doing.reconciling` task (the `end_reconciling_of_cancelled_plans` loop removed from `one_pass`) | `run::a_pass_ends_a_task_a_plan_cancel_left_reconciling` | ✅ failed |
| TK0036-M1 | run the `commands`/`review` pair one after the other again (`run_pair` calls the two stages instead of the two threads) | `review::the_commands_and_review_stages_start_before_either_ends` (the commands wait times out and fails the round) | ✅ failed |
| TK0036-M2 | let a review failure decide the round when `commands` also failed (`finish_verification` swaps the two in its scan order) | `review::a_commands_failure_decides_the_round_when_the_review_fails_too` (the task returns to `todo.ready` with a review brief) | ✅ failed |
| TK0036-M3 | forward the gate's stop to only the first running stage (`request_stop` signals one slot) | `run::a_signal_to_the_gate_stops_both_stages_of_the_pair` (one stage outlives the 3 s deadline) | ✅ failed |
| TK0035-M1 | clear a refused integration record again (the old `_` arm) | `run::a_refused_integration_is_not_restarted_until_its_inputs_change` (the record is gone after the first pass) | ✅ failed |
| TK0035-M2 | drop the wall-time bound on the worktree-setup fetch (call `git::run` without the clock) | `run::a_hanging_fetch_ends_the_attempt_at_the_wall_time` (40 s deadline: `start` never returned) | ✅ failed |
| TK0035-M3 | sleep a whole `POLL` slice inside each heartbeat interval again | `work::a_one_second_stall_window_keeps_three_beats_inside_it` (mean gap 414 ms) | ✅ failed |
| TK0044-M1 | take a single review lock again (`SlotLock::acquire` with 1 slot) | `review::two_tasks_reviews_overlap_with_concurrency_two` (the two reviews' intervals do not overlap) | ✅ failed |
| TK0044-M2 | apply the `[gate.review.first]` settings to every review (force `first = true`) | `review::a_first_review_uses_the_first_settings_and_a_rework_uses_the_normal_ones` (the rework session times out and the fake records no third run) | ✅ failed |
| TK0044-M3 | skip the owned-flag check for `gate.review.first.exec_args` | `review::a_first_exec_arg_the_review_stage_owns_is_refused` (the session runs and the stage passes) | ✅ failed |
| TK0037-M1 | `start` refuses `todo.retry_wait` again (`attempt_ops::claim`'s guard goes back to `phase == "ready"`) | `attempt::start_claims_a_retry_wait_task_without_waiting_for_retry_at`, `attempt::start_refuses_a_retry_wait_task_when_every_slot_is_taken` (the second gets the wrong refusal, so its `limits.concurrency` assertion fails) | ✅ 2 failed |
| TK0037-M2 | the scheduling loop ignores `retry_at` and claims a `todo.retry_wait` task (`run::start_ready` drops its phase check) | `attempt::start_claims_a_retry_wait_task_without_waiting_for_retry_at`, `run::a_future_retry_makes_run_exit_with_it_waiting` | ✅ 2 failed |

TK0037-M1–M2 were run against `12da692f`, each applied to the committed file, with
`cargo build -p radian-tui --bin radian -p agent-extensions --bin mcp-fixture` before the run
(the sync tests refuse a binary older than its sources), restored from a copy under
`.radian/tk0037/` with `cp`, and compared byte for byte with `cmp`.

## M6 and the inherited `GIT_CONFIG_COUNT`

The first M6 run passed every test: the parent environment carried
`GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_n` with `core.hooksPath=/dev/null`, and without the
helper's `-c core.hooksPath=` that injected value disabled hooks, hiding the mutation. The
helper now clears `GIT_CONFIG_COUNT` and `GIT_CONFIG_PARAMETERS`, so the gate's own `-c`
settings and the configuration files are the only sources; the mutation is then caught by
the hook test. The test still exercises a real repository `core.hooksPath`, not the injected
one.

TK0044-M1–M3 were run on this branch, each from a copy under `.radian/mut/` restored with `cp` and compared with `cmp`.

## Not caught

None of the listed mutations survived. A stale remote-tracking ref after a successful fetch is
not covered: every test pushes through the same clone, so the local ref is already current.

M13–M17 were run against `47616937` (the scheduling-loop commit), each from a copy under
`.radian/mutation-backup/` restored with `cp` and compared with `cmp`.

M18–M19 were run against this branch's verification-candidate change, each from a copy under
`.radian/mutations/` restored with `cp`.

M20–M23 were run against the scheduling change (a pass starts a worker or a gate and does not
wait), each from a copy under `.radian/mut/` restored with `cp` and compared with `cmp`.
M22's first run survived: the extra gates all lost the verdict race, so the test's counter
stayed at one. The test now also requires the first pass's whole run to print exactly one
`verify started` line, which is what the lock is for.

M24–M26 were run against the gate records, the whole-life heartbeat and the stage stop, each
from a copy under `.radian/mut2/` restored with `cp` and compared with `cmp`.

M27–M28 were run against the wrapper's whole-life wall clock and the gate lock's `FD_CLOEXEC`,
each from a copy under `.radian/mut3/` restored with `cp` and compared with `cmp`.

M29 was run against the process groups and the SIGINT handlers, from a copy under
`.radian/mut4/` restored with `cp` and compared with `cmp`. With the gate back in the loop's
group, the loop's group SIGINT still reaches the gate and its SIGINT handler still stops the
stage, so `sigint_to_the_watch_group_leaves_the_worker_running` passes; the test that fails is
the one that signals the gate's own group, which no longer exists.

M30–M31 were run against `adfc7fa5`, each from a copy under `.radian/mut/` restored with `cp`
and compared with `cmp`. M30 is the old spin: the pass leaves the `started` record and starts a
one-shot `run`'s gate on every pass, so no `no_record` record is ever written and each run
prints another `verify started` line. M31 is the old silence: a wrapper that refuses its
arguments or finds the worktree lock held prints nothing and leaves no record, so reconcile
judges it by its fresh heartbeat and the concurrency slot stays held for the stall window.

M32–M36 were run against the review-increment change (the rework round, `pre_existing` and the
review rework), from copies under `.radian/mutations/` restored with `cp` and compared with
`cmp`. M33's main-merge test still passed under the mutation, because that test's own diff is
already restricted to the task's file; the rework-prompt test is the one that catches it. M36
is the same-commit hole: with the guard gone a `reverify` of an unchanged head is a rework
round with an empty increment, so an item reported 29 lines from the previous one is moved
and the stage passes.

M41–M43 were run against `d8777f47` (the proposed task and the review's follow-up), each from
a copy under `.radian/mutations/` restored with `cp` and compared with `cmp`. M41 turns the
follow-up into a `doing.running` attempt on the next pass; M42 leaves the plan `draft` after
the task's confirm; M43 adds a task from a `blocking` item that the passing review in the test
holds, which arc itself never writes (a non-empty `blocking` fails the stage).

The old stale-main test (`a_rejected_push_never_overwrites_the_remote_main`) cannot survive
the explicit main refspec: a main moved on the remote is now visible before the push, so the
push is a fast-forward. Its replacement, `a_main_that_keeps_moving_is_never_overwritten`,
proves main keeps every commit the other writer pushed, but a `--force` push is no longer
distinguishable there (the only non-fast-forward now needs a race between `merge_ready` and
the push). The code still never forces.

M37–M40 were run against `fd0651da` (M37–M39) and `f00e4c6c` (M40), each from a copy
under `.radian/mut-auto/` restored with `cp` and compared with `cmp`. The ordinary repeat of
an automatic reverify is already stopped by its own condition (the new failing verdict
records the main it ran against, so "main moved since the failing verdict" is false); M37
therefore drives the state a pass leaves when it dies between recording the `(head, main)`
pair and claiming the attempt, which is the state the pair guard alone protects.

M44–M46 were run against `a948a2d7` (the fresh worker session), each from a copy under
`.radian/mut/` restored with `cp` and compared with `cmp`, with `cargo build -p radian-tui
--bin radian -p agent-extensions --bin mcp-fixture` before each run (the work tests refuse a
binary older than its sources). M44 is caught at the claim: a fresh rework claims
`<task>-2` with `resume` set, and the retry test and the review rework test see it too. M45
leaves the fresh brief as the body alone, so the hand-off tests fail on the missing section,
the missing cap and the missing hand-off in the conflicted brief. M46 swaps the two
sections in `compose_brief`'s argument list; only the conflicted-brief test compares their
order (the unit test passes the order it asserts). The `run::a_watch_pass_does_not_restart_a_gate_that_left_no_record`
failure that M45's run also showed is a timeout under load: it passes in every clean run.

M47 was run against `3ebead45`, from `.radian/mut/work_mod_walkback.rs` restored with `cp` and
compared with `cmp`: `newest_report` reads `attempt - 1` instead of walking back, so the
reverify attempt in the middle hides attempt 1's report and only the new work test fails.
M41–M43 were run against `a95de60e` (the priority change), each through `git restore` of the
mutated file and verified byte for byte with `git diff --exit-code`. M41 leaves the id order
alone, so `two_plans_at_the_same_priority_keep_id_order` still passes under it: the priority
tests and the `list` order catch it. M42 claims every ready task, so all three priority tests
fail, the same-priority one included. M43's parse and command checks are one decision in two
places; both are dropped in the one mutation, and the unit, store and CLI tests catch it.

M48–M50 were run against `9d3fdf67` (the retry count and the conflict limit), each from a copy
under `.radian/mutations/` restored with `cp` and compared with `cmp`. M49 is caught by one
test only: a task whose retry count is still 0 cannot show the missing reset, so the ten-round
test is the one that sees the count cross the rework. M50 leaves the run record itself in
place (each conflict still raises the run); only the stop is gone, so all three conflict-limit
tests fail and every other conflict test still passes.

M51–M52 were run against `4b6a9619`, each from a copy under `.radian/mutations/` restored with
`cp` and compared with `cmp`. M51 is what the old conflict path did; it leaves the conflict
limit itself in place, so only the test for an old-last-attempt conflict fails. M52 is the
JSON field alone: the text form still prints `retry_count`, so the drift-style tests pass and
the CLI test catches it.

M53–M54 were run against `4b6a9619` the same way (`cp` back, `cmp`). Each drops one reset and
one test sees it; no other test depends on the record, so the pair is what pins both reset
paths.

M50 was run again, and M55 was run, against the conflict budget's second rule (three briefs
before the fourth conflict stops; the lead's rework resets the run), when M50's replacement
was changed from "count the conflicts" to "count the conflict reworks arc wrote", each from a
copy under `.radian/mutations/` restored with `cp` and compared with `cmp`. M55 is caught by
`a_lead_rework_resets_the_conflict_run` alone; the run record it leaves shows `reworks: 3`.

M56–M60 were run against `73e6ce26` (the gate deaths, `interrupted`, and the stuck-gate
`reverify`), each applied to the committed file and restored with `git checkout --`, with
`git status --short` empty afterwards and `cargo build -p radian-tui --bin radian -p
agent-extensions --bin mcp-fixture` before every run (the sync tests refuse a binary older
than its sources). M56 is the old spin: a dead `running` record starts another gate on every
pass, so both death tests run to their 60 s deadline without ever seeing `died`. M57 is
caught by the first assertion of its test (the record still says `running`, and the restart
would carry a death); M58 is the old refusal, caught by the two `no_record`/`died` reverify
tests only, and its reverse (`gate_stuck` true for every `review.verifying` task) is what
`reverify_refuses_a_verifying_task_whose_gate_runs` pins. M59 leaves the gate of a
just-started task alone: it survives the loop, finishes its 15 s test-only delay, and writes
`done`. M60 is the item (d) mutation: with no start time it waits the full grace and then the
kill window, so the test's 2 s bound fails at 15 s.

## TK0033: a fast-forward merge shows the whole branch (2026-10-23, branch `work/TK0033-tui-pages-continued`)

`read::task_diff` used `<candidate>^1..<candidate>` for every merged task. When the
integration candidate is the task head (main had not moved), that range is only the branch's
last commit; the fix uses the verdict's `base_sha..head_sha` then and keeps the first-parent
diff for a merge candidate. The mutation deletes the fast-forward branch. It was applied to
the committed file, restored with `git checkout --`, and `git status --short` was empty
afterwards. The test lives in `radian-tui`, where the diff tab is drawn.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0033-tui-pages-continued-M2 | a merged task always diffs `<candidate>^1..<candidate>`, even when the candidate is the head | ✅ 1 failed | `radian-tui::arc_tui a_fast_forward_merged_task_shows_every_commit_of_its_branch` (`first.txt`, the branch's first commit, is missing from the file list) |

## TK0034 (cancel a plan)

TK0034-M1–M3 were run against `de2a816f`, each applied to the committed file, restored
from a copy under `.radian/mut/` with `cp`, and compared byte for byte with `cmp`. M1
removes both `plan.auto = "off"` writes in `cancel_plan`; the plan test sees `auto` still
`run`. M2 removes the cancelled-plan skip from `render`; the list test prints the plan
without `--all`. M3 is one logical change in two places: the loop's plan-status condition
in `start_ready` and the claim transaction's plan-status refusal in `attempt_ops::claim`.
The claim transaction's check alone would mask the loop's filter, so the run test's ready
task is claimed only when both are gone; the same mutation also fails the draft-plan claim
test, which pins the transaction's side. The run test's second half (`verify started` is
absent) pins the separate `verifying_tasks` cancelled-plan filter.

TK0034-M4 was run against the code of `2bd7e155`, with the same backup, `cp` restore and
`cmp` check. The whole step is removed from `one_pass`, so the pass prints `no transition`
and the task stays at `doing.reconciling`; the test's line assertion fails. The binary was
rebuilt before the run, because the sync tests refuse one older than its sources.
TK0036-M1–M3 were run against the parallel pair, each from a copy under `.radian/mut/`
restored with `cp` and compared with `cmp`, with `cargo build -p radian-tui --bin radian`
before each run (the `run` tests refuse a binary older than its sources). M1's sequential pair
makes the `commands` script wait out its 5 s barrier and fail before the review starts, so the
round is `review.rework_pending` instead of `awaiting_human`. M2 is caught only when both
stages fail: with `commands` failing alone the review passes, and scanning it first changes
nothing. M3 leaves the stage that was not signalled alive past the test's 3 s deadline; the
handler is the only SIGTERM source, because the wait loop now only escalates to SIGKILL after
`cancel_grace_secs`.

## TK0051 (a critical review item always blocks; an unplaced item is not demoted)

TK0051-M1–M4 were run against `a7833564`, each applied to the committed file, restored from a
copy under `.radian/mut/` with `cp`, and compared byte for byte with `cmp`; `git status --short`
was empty afterwards. M1 disables the `critical` early return in `ReworkRound::keeps`, so the
window rule moves a critical item again: the unit test finds it in `suggestions` and the stage
test passes the round and reaches `awaiting_human`. M2 returns `false` for an item with no
`file` or no `line`, so an unplaced item is demoted again: the `vague` item of the mixed unit
test moves, and all three items of the unplaced test move. M3 writes `level: None` on a moved
item, so the demoted `major` item loses its grade in the verdict and in the follow-up brief.
M4 keeps only the newest `review` verdict in `review_verdicts` (the follow-up's input), so every
earlier round goes unread and the merge proposes no follow-up. It was re-run on the merged
follow-up code, after TK0045 replaced the single-verdict read.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0051-M1 | let the window rule move a `critical` item again (`if false && level == CRITICAL`) | ✅ 2 failed | `gate::review::tests::a_critical_item_blocks_wherever_it_is`; `review::a_critical_item_far_from_the_rework_and_the_previous_items_still_blocks` |
| TK0051-M2 | demote an item without a `file` or a `line` again (the `let … else` returns `false`) | ✅ 2 failed | `gate::review::tests::an_item_without_a_place_stays_blocking_at_its_level`; `gate::review::tests::a_pre_existing_item_is_moved_and_marked` |
| TK0051-M3 | drop the `level` of a moved item (`level: None` in `mark_pre_existing`) | ✅ 2 failed | `gate::review::tests::a_major_item_outside_the_windows_is_moved_and_keeps_its_level`; `review::a_demoted_major_item_reaches_the_followup_at_its_level` |
| TK0051-M4 | read only the newest review verdict again (`review_verdicts` keeps just its last entry) | ✅ 3 failed | `review::a_major_item_a_failing_round_demoted_reaches_the_followup`; `review::a_pre_existing_item_from_an_earlier_round_is_carried`; `review::a_followup_holds_every_round_grouped_and_drops_a_repeat` |
## TK0045 (the follow-up holds every review round)

TK0045-M1–M4 were run on the branch's working code before the code commit, each from a copy
under `.radian/`, restored with `cp` and compared with `cmp`; the restored files match the
commit. M1 turns the new `review_verdicts` read back into
`latest_review_verdict`, so every test that needs an earlier round loses its item. M2 removes
the `find`/replace from `followup_items`. M3 writes `self.text` again in `ReviewItem`'s
`Display`. M4 removes the `is_followup` early return from `propose_followup`.

| # | Mutation | Caught by | Result |
|---|---|---|---|
| TK0045-M1 | read only the newest `review` verdict again (`latest_review_verdict`) | `review::a_followup_holds_every_round_grouped_and_drops_a_repeat`, `review::a_round_with_only_blocking_items_carries_nothing`, `review::a_pre_existing_item_from_an_earlier_round_is_carried` | ✅ 3 failed |
| TK0045-M2 | drop the deduplication (every round's item is carried) | `review::a_followup_holds_every_round_grouped_and_drops_a_repeat` (the repeat is under both rounds) | ✅ failed |
| TK0045-M3 | keep the duplicated level tag in `Display` (write `self.text`) | `review::a_rework_brief_prints_a_duplicated_level_tag_once`, `review::a_followup_brief_prints_a_duplicated_level_tag_once` | ✅ 2 failed |
| TK0045-M4 | propose a follow-up for a merged follow-up task (drop the `is_followup` guard) | `review::a_merge_of_a_followup_task_proposes_no_followup` (it gets `TK0003-…-followups-followups`) | ✅ failed |

## TK0019 (the running task's live diff and `loop.log`)

TK0019-M3 mutates `read::task_diff`'s live-worktree branch (`radian-arc/src/read.rs`); the test
lives with the page that draws it (`radian-tui/tests/arc_tui.rs`). TK0019-M7 and M8 mutate the
`loop.log` writer in `radian-arc/src/run.rs`; their rows are recorded in
`crates/radian-tui/MUTATIONS.md` with M1–M6 so the task's numbering stays in one place.

| # | Mutation | Caught by | Result |
|---|---|---|---|
| TK0019-M7 | the rotation keeps appending to the 1 MiB file (`rotate_if_needed` does nothing) | `run::a_full_loop_log_is_rotated_to_loop_log_1` (`loop.log.1` is not the old content; the new file holds `old line`) | ✅ failed |
| TK0019-M8 | `run` without `--watch` writes no `loop.log` (`run` calls `run_quiet` without the `LoopLog` sink) | `run::run_appends_every_printed_line_to_loop_log` (no `loop.log`) | ✅ failed |

## TK0035 (a refused integration, the wrapper setup and the heartbeat)

TK0035-M1–M3 were run against `29d77904`, each applied to the committed file, restored from a
copy under `.radian/mut/` with `cp`, and compared byte for byte with `cmp`, with
`cargo build -p radian-tui --bin radian -p agent-extensions --bin mcp-fixture` before every
run (the run tests drive the `radian` binary). M1 clears the refused record before the input
comparison, so the first pass drops it and the test's record assertion fails (the second pass
would then start the same gate again). M2 replaces the fetch's clock with an unbounded
`git::run`, so `start` never returns and the test's 40-second deadline panics. M3 is the old
heartbeat loop; the mean gap over three beats is 414 ms, where a 1 s window needs 333 ms.
