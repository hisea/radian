# `radian-coding` mutation record (Phase 3 coding tools)

Base: `0a8dda4` · one-shot worktree `/tmp/tools-mut` (created from HEAD, deleted after) ·
every mutation run with `cargo test -q -p radian-coding --no-fail-fast`; `git checkout --`
after each; `git status --short` empty at the end.

| # | Requirement mutated | Mutation | Result |
|---|---|---|---|
| M1 | 1 absolute path escape | remove **both** overlapping absolute-path checks (`is_absolute()` and the `Component::RootDir` arm) | ✅ caught (`an_absolute_read_path_is_refused`, `an_absolute_path_inside_the_workspace_is_still_refused`, `write_absolute_and_dotdot_paths_are_refused`) |
| M2 | 2 `..` escape | drop `Component::ParentDir` from the component check | ✅ caught (`parent_dir_escapes_are_refused`) |
| M3 | 3 symlink escape (read) | remove `ensure_inside` from `resolve_existing` | ✅ caught (`a_symlink_pointing_outside_is_refused_for_read`) |
| M4 | 4/9 symlinked parent (write) | remove `ensure_inside` from `resolve_parent_for_write` | ✅ caught (`write_into_an_outside_symlinked_directory_is_refused`) |
| M5 | 5 write through an existing link | drop the `guard_mutation_target` call in `write` | ✅ caught (`write_through_a_symlink_to_outside_is_refused`, `a_link_swapped_in_after_a_write_is_refused_by_the_next_write`) |
| M6 | 6 hard link | make the `nlink > 1` check unreachable | ✅ caught (`write_and_patch_refuse_a_hard_linked_file`) |
| M7 | 7 TOCTOU (`O_NOFOLLOW`) | remove `custom_flags(O_NOFOLLOW)` from the write open | ❌ **not caught** — see below |
| M8 | 8 symlinked workspace root | stop canonicalizing the root in `WorkspaceRoot::new` | ✅ caught (`a_symlinked_workspace_root_works_and_still_confines`) |
| M9 | read size limit | make the `max_bytes > max_read_bytes` check unreachable | ✅ caught (`read_refuses_a_budget_above_the_hard_cap`) |
| M10 | read size limit | ignore the byte budget (`.take(u64::MAX)`) | ✅ caught (`read_reports_truncation_and_the_total_size`) |
| M11 | `patch` auditability | let multiple matches fall through and edit anyway | ✅ caught (`patch_refuses_an_ambiguous_needle_and_leaves_the_file_alone`) |
| M12 | `.git` must not be written | remove **both** `--no-optional-locks` and `GIT_OPTIONAL_LOCKS=0` | ✅ caught (`git_status_and_diff_report_the_worktree_and_leave_dot_git_untouched`) |

## The two mutations that were not behaviour changes

- **M7 (`O_NOFOLLOW`)**: the destination symlink is already refused by `guard_mutation_target`
  before the open, so removing `O_NOFOLLOW` changes nothing a test can see. It is
  **defence in depth** against the destination being swapped for a link between the guard
  and the open; no deterministic test can produce that race, so this slice does not claim
  it is covered. The parent-directory window is not mitigated at all.
- **M1 first attempt**: removing only `is_absolute()` was an *equivalent* mutation, because
  the `Component::RootDir` arm of the second check refuses absolute paths too. Both must be
  removed to change behaviour. The record keeps this because it is the honest picture: the
  requirement is enforced by one of two overlapping checks, not by `is_absolute()` alone.

## A mutation that needed both mechanisms removed

**M12**: `--no-optional-locks` and `GIT_OPTIONAL_LOCKS=0` are equivalent switches; removing
either alone leaves git lock-free, so only removing both made the `.git` snapshot change
(the test's repo has a stale index stat after the modification, which is exactly when git
would refresh and write `.git/index`).

## Coverage added because of this pass

`an_absolute_path_inside_the_workspace_is_still_refused` was added: the rule is
"workspace-relative only", so an absolute path that happens to point inside must be refused
too. Without it, the absolute-path requirement had no test that distinguished it from the
canonicalization check.

## `shell` tool (Phase 3 sandbox slice)

Base: `fd0388d` (re-run at the final tests commit) · one-shot worktree `/tmp/shell-mut` (created from HEAD, deleted after) ·
each mutation run with `cargo test -p radian-coding --test shell <test> -- --exact
--no-fail-fast`; files restored after each; `git status --short` empty at the end; no orphan
processes left (`ps` check).

| # | Requirement mutated | Mutation | Result |
|---|---|---|---|
| M1 | no sandbox ⇒ no shell, no naked run | the `None` execute arm runs `/bin/sh -c` directly instead of refusing | ✅ caught (`shell_is_absent_without_a_sandbox`) |
| M2 | timeout/cancel kill the whole process group | `killpg(pid)` → `kill(pid)` (only the direct child) | ✅ caught (`a_timeout_kills_the_whole_process_group`; the background `sleep 600` survives) |
| M3 | output cap | `take_from_budget` returns `want` and ignores the budget | ✅ caught (`output_over_the_cap_is_truncated_and_reported`) |
| M4 | environment filtered in every mode | remove the `env_clear` + whitelist block for the transparent backend | ✅ caught (`the_host_environment_does_not_reach_the_shell`) |
| M5 | `cwd` stays inside the workspace | `root.resolve_existing(dir)` → `root.root().join(dir)` (no canonicalize/containment) | ✅ caught (`cwd_escapes_are_refused`) |
| M6 | `full-access` is never a default | `CodingTools::new` builds a transparent `FullAccess` sandbox internally | ✅ caught (`full_access_is_never_the_default`) |
| M7 | a failing backend never downgrades | `Sandbox::from_backend` falls back to a transparent sandbox when `probe()` fails | ✅ caught (`an_unavailable_backend_never_yields_a_sandbox_or_a_shell`) |

7/7 caught; no misses. M2 is the one that shows the "kill the direct child only" bug is a
real hazard: with it the background `sleep 600` is still alive after the timeout.

## `shell` output truncation (follow-up: head+tail, per stream)

Base: `e0fd49a` · one-shot worktree `/tmp/shell-mut2` (created from HEAD, deleted after) ·
each mutation run with `cargo test -p radian-coding --test shell <test> -- --exact
--no-fail-fast`; files restored after each; `git status --short` empty at the end.

The first seven mutations were re-run against the new reader (M3 now removes the caps
entirely rather than the old shared budget), plus three for the new behaviour:

| # | Requirement mutated | Mutation | Result |
|---|---|---|---|
| M1–M2, M4–M7 | as above | as above | ✅ caught (same tests) |
| M3 | output cap | `head_cap = usize::MAX; tail_cap = 0` (keep everything) | ✅ caught (`output_over_the_cap_is_truncated_and_reported`) |
| M8 | keep the tail | `tail_cap = 0` (head only) | ✅ caught (`the_end_of_a_long_output_is_kept`; the final `ERROR:` line is gone) |
| M9 | per-stream budget | stderr is read with budget `0` — the observable consequence of one shared budget already spent by stdout | ✅ caught (`each_stream_has_its_own_budget`) |
| M10 | render the tail | drop the tail from `Capped::render` (keep head + marker) | ✅ caught (`the_end_of_a_long_output_is_kept`) |

10/10 caught; no misses. M9 is an equivalent mutation rather than a literal shared-budget
rewrite: with one budget, a flooding stdout leaves stderr nothing, which is exactly what
`read_capped(stderr, 0)` models, and the test asserts the stderr line survives.

Memory is not asserted by a mutation but by a unit test (`the_reader_keeps_a_bounded_head_and_tail`,
50 MiB through a 64 KiB cap) and an end-to-end one (`memory_does_not_grow_with_the_output`,
50 MiB through a real process: peak RSS grew by ~688 KiB, measured with `getrusage`).

## ADR 0016：shell 的 setsid（2026-10-13）

一次性检出 `/tmp/tiers-mut`（base `9c060d2`），`--no-fail-fast`，变异后 `git checkout --` 恢复。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M10 | 去掉 `shell.rs` 的 `pre_exec(setsid)`（既不 setsid 也不建进程组） | ✅ failed | `a_timeout_kills_the_whole_process_group`、`cancellation_kills_the_whole_process_group`（`killpg` 打不到子进程组） |

**注意**：M10 不是被 `tiocsti_outside_terminal` 抓住的。逃逸矩阵不经过 shell 工具，而两个后端
（macOS seatbelt / Linux bwrap）各自就会拒 `TIOCSTI`，所以矩阵对 setsid 不敏感（详见
`radian-sandbox/MUTATIONS.md` 的 LM6）。setsid 的可观测后果是**进程组清理**，由上述两个用例钉住。

## ADR 0024：`ask_user` 的工具一半（2026-10-13）

Base `4866944` · 一次性检出 `/tmp/ask-mut`（detached，跑完删除）· 独立 target ·
`cargo test -p radian-coding --test ask`（A3 另加 `--test specs`）· 每个变异后从副本还原，
结束时 `git status --short` 只剩变异脚本本身。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| A1 | 取消时构造一个空答案（`Ok`）而不是 `cancelled` | ✅ 抓住 | `a_cancelled_ask_is_cancelled_and_never_answers` |
| A2 | `QuestionRequest::validate` 直接返回 `Ok(())` | ✅ 抓住 | `every_question_limit_is_an_invalid_arguments_failure` |
| A3 | 没有 prompter 也把 `ask_user` 放进 specs | ✅ 抓住 | `without_a_prompter_the_tool_is_not_advertised`、`specs::agent_core_accepts_the_built_in_tool_specs` |
| A5 | `PromptError::Declined` 映射成 `prompter_gone` | ✅ 抓住 | `a_declined_ask_is_user_declined` |
| A6 | `validate_answer` 直接返回 `Ok(())`（不查 id 一一对应） | ✅ 抓住 | `an_answer_that_does_not_match_the_questions_is_rejected` |
| A10 | `ask_user` 的 concurrency 从 `Exclusive` 改成 `Shared` | ✅ 抓住 | `with_a_prompter_the_tool_is_advertised_and_exclusive` |
| A11 | 人读 `content` 用 option id 而不是 label | ✅ 抓住 | `the_answer_is_structured_json_and_readable_text` |

7/7 抓住。A1 值得注意：变异让取消分支返回 `Ok`，测试断言的是 `Err(cancelled)`，
所以"绝不构造答案"是被钉住的行为，不是注释。
