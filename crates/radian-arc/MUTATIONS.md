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

The old stale-main test (`a_rejected_push_never_overwrites_the_remote_main`) cannot survive
the explicit main refspec: a main moved on the remote is now visible before the push, so the
push is a fast-forward. Its replacement, `a_main_that_keeps_moving_is_never_overwritten`,
proves main keeps every commit the other writer pushed, but a `--force` push is no longer
distinguishable there (the only non-fast-forward now needs a race between `merge_ready` and
the push). The code still never forces.
