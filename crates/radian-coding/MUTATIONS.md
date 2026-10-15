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

## read：按行 offset/limit 与行边界截断（dogfood10 D25）

Base `e1dccf0` · 在本任务工作树内逐条施加后从副本还原（`--no-fail-fast`）· `git status --short`
结束后干净。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| R1 | 丢行时的 `next_offset` 少 1（`lineno` → `lineno - 1`） | `read::a_budget_cut_mid_line_resumes_at_that_line`、`read::a_budget_ending_exactly_on_a_character_keeps_it`、`read::next_offset_reads_the_whole_file` | ✅ failed |
| R2 | 预算在行中间耗尽时返回半行（不丢整行，直接续到下一行） | `read::a_budget_cut_mid_line_resumes_at_that_line`、`read::a_budget_ending_exactly_on_a_character_keeps_it`、`read::next_offset_reads_the_whole_file` | ✅ failed |
| R3 | 忽略 `offset`（恒从第 1 行开始） | `read::read_starts_at_a_middle_line`、`read_limit_stops_after_n_lines`、`an_overlong_first_line_is_returned_as_a_marked_prefix`、`an_offset_past_the_end_is_empty_with_a_note`、`a_non_positive_offset_or_limit_is_invalid_arguments`、`next_offset_reads_the_whole_file` | ✅ failed |
| R4 | 丢一行（不把第一个被接受的行写进输出） | `read::next_offset_reads_the_whole_file`（拼接少一行）等 6 个 | ✅ failed |

4/4 抓住。R1/R2 让 `next_offset` 循环读完整文件的拼接与原文件不一致，正是这条测试要防的
"少一行/多一截"。

### read-2：line_truncated 的正文提示，与读取阶段的内存上界

Base `4538c14` · 在本任务工作树内逐条施加后从副本还原（`--no-fail-fast`）· `git status --short`
结束后干净。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| R5 | `line_truncated` 时正文不加提示（只有非 line_truncated 才加） | `escaping::read_reports_truncation_and_the_total_size`、`read_trims_an_incomplete_character_at_the_budget_edge`、`read::an_overlong_first_line_is_returned_as_a_marked_prefix`、`a_multibyte_character_on_the_budget_edge_is_not_split`、`a_huge_single_line_is_bounded` | ✅ failed |
| R6 | 读取阶段改回 `read_until` 整行读入内存 | `read::tests::the_read_phase_never_reads_more_than_the_budget_plus_a_buffer`（计数 reader 看到读了 64 MiB） | ✅ failed |

2/2 抓住。R6 的计数 reader 用有限的 64 MiB `repeat`（不是无限，否则变异会挂而不是失败），断言读取阶段从源拉取的字节 ≤ `budget + BufReader 缓冲区`；跳过阶段只扫换行、常数内存，不受这条约束。

## review-fixes 片：`read` 首行截断时吞掉非法 UTF-8（P1）

Base `c120b0e` · 就地变异（改 `utf8_prefix`），跑完从备份还原、`git status --short` 为空。

| # | 变异 | 应体现 | 抓住它的测试 |
|---|---|---|---|
| F1 | `utf8_prefix` 恢复成"一律 `valid_up_to()`"（不区分 `error_len()`） | 预算内的确定非法字节必须拒绝，而不是悄悄截成"看着正常"的文本 | `read::tests::an_over_long_first_line_with_an_invalid_byte_is_refused` FAILED（行首 `0xFF` 那半也 FAILED） |

1/1 抓住。合法的"预算切在多字节字符中间"由
`an_over_long_first_line_cut_mid_character_keeps_a_valid_prefix` 钉住（变异 F1 下该用例仍过,
因为它本来就走前缀分支）；"非法字节在预算之外不失败"由
`an_invalid_byte_beyond_the_budget_does_not_fail_the_read` 钉住。

## bg-jobs：后台 shell 命令与三个 job 工具（ADR 0037 切片 1-2，2026-10-16）

Base `7978228`（J1-J6）、`80c158b`（J7）· 一次性 detached worktree `/tmp/bg-mut`、`/tmp/bg-mut2`
（跑完 `git worktree remove --force`）。
每个变异用 `cargo test -p radian-coding --no-fail-fast -- <test> --exact`（会重新编译），
还原后 `git status --short` 为空（只留未跟踪的备份与脚本，随 worktree 一起删掉）。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| J1 | `job_output` 的 wait 超时顺手 `terminate`（杀掉 job） | `a_wait_timeout_returns_running_and_leaves_the_job_alive` | ✅ |
| J2 | `signal_group` 用 `kill` 而不是 `killpg`（只杀 leader） | `job_kill_kills_the_whole_process_group` | ✅ |
| J3 | 并发上限检查关掉（`if false && running >= MAX_RUNNING_JOBS`） | `the_fifth_running_job_is_refused` | ✅ |
| J4 | 取消 turn 时顺手 `terminate`（杀掉 job） | `a_cancelled_turn_leaves_the_job_running` | ✅ |
| J5 | `kill_all` 不再发信号（session-end 清理跳过） | `session_end_cleanup_kills_every_running_group` | ✅ |
| J6 | `Ring::since` 忽略游标、每次返回整个 ring | `jobs::tests::the_ring_keeps_the_last_bytes_and_reports_the_increment` | ✅ |
| J7 | `supervise` 去掉 leader 退出后的 `signal_group(pid, SIGKILL)`（自然退出不杀组） | `a_natural_exit_kills_a_background_grandchild` | ✅ |

7/7 抓住。J1/J4 需要测试在 wait 返回后**再等一个有界窗口**（`gone_within`）才断言进程还活着：
只断言「立刻 alive」会被 SIGTERM 的投递竞态放过（第一次跑时 J1 就没抓住）；改后两者都确定性抓住。
J2 改的是共享的 `signal_group`，前台 `a_timeout_kills_the_whole_process_group` 也会抓，
这里记的是 jobs 测试。J6 由单元测试抓住（集成测试 `a_background_job_returns_at_once_and_job_output_returns_increments`
的「第二次读只返回增量」断言同样会抓）。

J7 是评审发现的洞：去掉 `signal_group(pid, SIGKILL)` 后原 9 个测试全绿（`job_kill` 与 timeout 自己
发信号，不经过这条路径）。新测试跑 `sleep 300 & echo $! > grandchild.pid; exit 0`，settle 成
`exited 0` 后断言孙子进程没了。同一变异下也观察到：孙子进程攥着 stdout/stderr，reader 的 EOF
要等 `READER_GRACE`（两个流各 2 s），所以 settle 被推迟约 4 s（测试 14.05 s vs 修好后 0.02 s）；
正确代码里先杀组、管道随即关闭，settle 不延迟。

## bg-jobs-2：完成通知、journal、/jobs、崩溃（ADR 0037 切片 3-4，2026-10-16）

Base `eb7ec96`（N2/N5/N6/N7）、`cefd46e`（N8/N9）· 一次性 detached worktree `/tmp/bg2-mut`、`/tmp/bg2b-mut`
（跑完 `git worktree remove --force`）。
每个变异用 `cargo test -p <pkg> --test <target> --no-fail-fast -- <test> --exact`（会重新编译），
还原后 `git status --short` 为空（只留未跟踪的备份与脚本，随 worktree 一起删掉）。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| N2 | settle 的 `notify` 忽略 `waiters`（被 `job_output wait=true` 等到的 settle 又通知一次） | `jobs::a_waited_settle_is_not_notified` | ✅ |
| N5 | `kill_all` 只杀第一个（session-end/`/jobs kill all` 留下其余的） | `jobs::session_end_cleanup_kills_every_running_group` | ✅ |
| N6 | `Drop for Jobs` 变成 no-op（panic 路径不清理） | `jobs::dropping_the_registry_kills_every_running_group` | ✅ |
| N7 | 前台 timeout 消息去掉 `background: true` 的提示 | `shell::a_timeout_kills_the_whole_process_group` | ✅ |
| N8 | `notify` 计算忽略 `Termination::Killed`（被 kill 的 job 也通知） | `jobs::the_notify_flag_follows_the_settle_kind` | ✅ |
| N9 | `kill_all` 不设 `termination = Killed`（session-end settle 读成 `signal 9` 并通知） | `jobs::a_session_cleanup_settle_is_not_notified` | ✅ |

6/6 抓住。N6 另有端到端 `radian-tui` 的
`pty_panic_path_does_not_orphan_a_background_job`（exit 101 + 组为空）覆盖；那里用
`--panic-with-background-job` 钩子先起 job 再 panic。
## glob / grep：两个 Shared 搜索工具（2026-10-15，backlog B1）

Base `236f4c3`（实现 `385d21d` + 接线 `18aeff7` + 测试 `236f4c3`）· 就地变异（改源码），
每个变异前把文件复制到工作树外的 scratchpad，跑完从副本还原并逐字节比较，结束时
`git status --short` 为空。

每个变异用 `cargo test -p <crate> --test <target> --no-fail-fast -- --exact <test>`（会重新编译）。

| # | 变异（行为） | 抓住它的测试 | 结果 |
|---|---|---|---|
| G1 | glob 不套用 pattern（遍历分支恒真） | `search::glob_matches_by_path_and_skips_non_matches` | ✅ |
| G2 | 不读 `.gitignore`（`.git_ignore(false)`） | `search::the_ignore_rules_are_respected_by_both_tools` | ✅ |
| G3 | 不剪枝 `.git`（`filter_entry` 恒真） | `search::the_dot_git_directory_is_skipped_by_both_tools` | ✅ |
| G4 | 跟随符号链接（`follow_links(true)`） | `search::a_symlink_out_of_the_workspace_is_never_followed` | ✅ |
| G5 | `glob` 的基路径不走边界解析（`root.root().join(path)`） | `search::the_boundary_refuses_a_path_outside_the_workspace` | ✅ |
| G6 | `grep` 的基路径同上 | `search::the_boundary_refuses_a_path_outside_the_workspace` | ✅ |
| G7 | `glob` 不截断结果（`truncate(usize::MAX)`） | `search::glob_caps_the_result_and_names_the_cut` | ✅ |
| G8 | `*` 跨目录（`literal_separator(false)`） | `search::a_single_star_stays_within_one_directory` | ✅ |
| G9 | `grep` 不套用任何匹配上限（budget 恒为 `usize::MAX`） | `search::grep_caps_the_matches_per_file_and_names_the_cut`（总数上限测试同样会抓） | ✅ |
| G10 | 非法正则的 code 从 `invalid_regex` 改成 `invalid_arguments` | `search::an_invalid_regex_is_a_named_tool_error` | ✅ |
| G11 | 忽略 `case_insensitive`（恒 `false`） | `search::grep_reports_matches_with_path_line_and_text` | ✅ |
| G12 | 不输出 after-context（`after_pending = 0`） | `search::grep_shows_context_lines_and_separates_groups` | ✅ |
| G13 | 二进制探测恒假（NUL 文件也搜） | `search::grep_skips_binary_files` | ✅ |
| G14 | `read_only()` 去掉 `glob`/`grep` | `radian-permissions::profile::tests::the_search_tools_are_allowed_in_every_tier` | ✅ |

14/14 抓住。G5/G6 是同一行为在两个工具里的两份实现：只变异其中一个，`the_boundary_refuses_a_path_outside_the_workspace`
里那个工具的 `unwrap_err()` 会因为不再报错而 panic，两个变异各自被同一个测试抓住。G9 只改
`budget` 的计算：`per_file_cap` 与 `cap` 都不再生效，per-file 与 total 两条上限测试都会失败，
这里记的是 per-file 那条。G13 用 `if false` 而不是删掉整个 `sniff` 块：删块会让 `sniff`/`filled`
变成未使用（只是警告），而 `if false` 让分支在编译期仍然存在、行为上关闭探测，语义更接近"关掉这个
判断"。
