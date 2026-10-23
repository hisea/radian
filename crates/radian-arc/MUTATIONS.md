# `radian-arc` mutation record (gate slice, ADR 0048)

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

## M6 and the inherited `GIT_CONFIG_COUNT`

The first M6 run passed every test: the parent environment carried
`GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_n` with `core.hooksPath=/dev/null`, and without the
helper's `-c core.hooksPath=` that injected value disabled hooks, hiding the mutation. The
helper now clears `GIT_CONFIG_COUNT` and `GIT_CONFIG_PARAMETERS`, so the gate's own `-c`
settings and the configuration files are the only sources; the mutation is then caught by
the hook test. The test still exercises a real repository `core.hooksPath`, not the injected
one.

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

M41–M43 were run against `a95de60e` (the priority change), each through `git restore` of the
mutated file and verified byte for byte with `git diff --exit-code`. M41 leaves the id order
alone, so `two_plans_at_the_same_priority_keep_id_order` still passes under it: the priority
tests and the `list` order catch it. M42 claims every ready task, so all three priority tests
fail, the same-priority one included. M43's parse and command checks are one decision in two
places; both are dropped in the one mutation, and the unit, store and CLI tests catch it.
