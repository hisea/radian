# `radian-coding` mutation record (Phase 3 coding tools)

New rows use the id `<task id>-M<n>` (for example `TK0034-M1`); see "Mutation records" in `AGENTS.md`.

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
| A3 | 没有 prompter 也把 `ask_user` 放进 specs | ✅ 抓住 | `without_a_prompter_the_tool_is_not_advertised`、`specs::radian_core_accepts_the_built_in_tool_specs` |
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

## tool-details：read 行号前缀（2026-10-15，backlog P5）

Base `ea8d251` · 就地变异（改源码），每个变异前把文件复制到工作树外的 scratchpad，跑完从副本
还原并逐字节比较，结束时 `git status --short` 只剩本片自己的改动。每个变异用
`cargo test -p radian-coding --test read --no-fail-fast -- <test>`（会重新编译）。

| # | 变异（行为） | 抓住它的测试 | 结果 |
|---|---|---|---|
| L1 | `number_lines` 的行号从 `start_line + index` 变成 `start_line + index + 1`（编号整体加一，中段 offset 不再对应用户给的行） | `read::read_prefixes_each_line_with_its_file_line_number`（中段读期望 `3: gamma`，变异给出 `4: gamma`） | ✅ |
| L2 | `suggest_similar` 列目录从「请求文件所在目录」改成「工作区根」 | `read::a_missing_read_suggests_a_similar_name_from_the_same_directory`（应为 `deep/readme.md`，变异给出根目录的同名 `README.md`） | ✅ |

### patch 分层匹配与 replace_all（piece 3）

每个变异用 `cargo test -p radian-coding --test patch --no-fail-fast -- <test>`，跑完从副本还原并逐字节比较。

| # | 变异（行为） | 抓住它的测试 | 结果 |
|---|---|---|---|
| P3-1 | 第 2 层（排版字符归一化）只取第一个匹配（`take(1)`），非唯一也接受 | `patch::a_normalised_match_must_still_be_unique`（期望 `patch_ambiguous`，变异返回 Ok） | ✅ |
| P3-2 | 写入前把结果里的 `\r\n` 全部换成 `\n`（CRLF 文件被改成 LF） | `patch::a_crlf_file_matches_an_lf_needle_and_stays_crlf`（期望字节含 `\r\n`） | ✅ |

### patch 未命中时返回最近区域（piece 4）

| # | 变异（行为） | 抓住它的测试 | 结果 |
|---|---|---|---|
| P4-1 | `closest_region` 直接返回 `None`（错误里不再给出最近的行区间） | `patch::a_missing_needle_reports_the_closest_region_with_line_numbers`（期望含 `lines 2-3` 与 `2: beta`） | ✅ |

### 参数错误：列出全部问题、回显参数、空字符串可选参数视为缺省（piece 5）

每个变异用 `cargo test -p radian-coding --test args --no-fail-fast -- <test>`，跑完从副本还原并逐字节比较。

| # | 变异（行为） | 抓住它的测试 | 结果 |
|---|---|---|---|
| A1 | `clean` 不再删除值为 `""` 的可选参数（把 `""` 当成一个值） | `args::an_empty_optional_argument_is_treated_as_absent`（`offset: ""` 变成类型错误） | ✅ |
| A2 | `invalid` 只打印第一个问题（`.take(1)`） | `args::an_argument_error_lists_every_problem_and_echoes_the_arguments`（三个问题只出现一个） | ✅ |

### shell 工具描述：读文件与搜索用只读工具（2026-10-15）

Base `e19e6cf`（worktree `.radian-worktrees/mode-cmd`）· 变异就地改完后跑
`cargo test -p radian-coding --test shell --no-fail-fast -- the_shell_description`，再从
`/tmp/mut/shell.rs.bak` 还原并用 `diff -q` 确认逐字节相同。

| # | 变异（行为） | 抓住它的测试 | 结果 |
|---|---|---|---|
| SD-1 | 删掉描述里的 `Read files with read and list, and search with grep and glob … while shell runs alone`（回到只提 "Use `shell` to run programs"） | `the_shell_description_points_file_reading_and_searching_at_the_read_only_tools`（三段关键句都断言） | ✅ |

## ADR 0039 决定 1/6：后台任务输出移出工作区、事件有序（2026-10-16）

工作树 `oc/sec-jobs`（base `0e9baef`）· 变异就地改源码、跑完从
`.radian/scratch/mut/` 的副本还原并 `diff -q` 确认，`git status --short` 只剩本片自己的改动。
每个变异跑对应的测试目标（`--no-fail-fast`）。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| SJ1 | `open_job_output` 的 `create_new(true)` 改成 `create(true)`（保留 `O_NOFOLLOW`） | `job_output_security::an_existing_output_file_at_the_next_id_is_not_appended_to`（往已存在的文件里追加） | ✅ failed |
| SJ2 | 去掉 `custom_flags(O_NOFOLLOW)`（保留 `create_new`） | —（等价变异，见下） | ❌ 未抓住 |
| SJ3 | 同时去掉 O_EXCL 与 O_NOFOLLOW（ADR 的「去掉 O_EXCL/O_NOFOLLOW」） | `job_output_security::a_planted_symlink_at_the_next_output_file_is_not_followed`（`start` 顺着 symlink 打开，输出落在外部文件） | ✅ failed |
| SJ4 | 宿主把 jobs 目录改回工作区内（`dir.join(".radian/jobs")`） | `radian-host::session::tests::the_jobs_directory_is_the_sessions_own_and_owner_only` | ✅ failed |

**SJ2（等价变异，诚实记录）**：POSIX 规定 `O_CREAT|O_EXCL` 对已存在的路径（含 symlink）返回
`EEXIST`；本机 macOS 实测：`O_WRONLY|O_CREAT|O_EXCL|O_NOFOLLOW` 与 `O_WRONLY|O_CREAT|O_EXCL`
对 symlink 都返回 `EEXIST`；`O_NOFOLLOW` 只在 `O_CREAT|O_APPEND`（无 `O_EXCL`）时才把 symlink 变成
`ELOOP`。所以只留 `create_new` 时 `O_NOFOLLOW` 无可观察后果。ADR 0039 把
`O_EXCL/O_NOFOLLOW` 当一个变异（SJ3 抓住）；单独去掉 `O_NOFOLLOW` 是纵深防御，没有确定性测试能
区分它，与上面 M7（`write` 的 `O_NOFOLLOW`）同理。

SJ5（supervisor 在 `Started` 之前 spawn）见下方「决定 6」小节。

## ADR 0037 decision 6：先登记并发 `Started`，supervisor 之后才 spawn（2026-10-16）

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| SJ5 | `Jobs::start` 把 insert + `Started` 放回 supervisor/timer spawn 之后（本片前的顺序） | `radian-host::job_notify::a_burst_of_instant_jobs_keeps_every_settle_behind_its_start`（第 117 个 job 现 `job-81330-117 settled before it started`） | ✅ failed |

SJ5 要在 1000 次瞬时 job 里跑出一次线程调度倒序：`Started` 与 `Settled` 由不同线程发出，主线程在
spawn supervisor 之后还要 insert 并 `send`，所以 supervisor 偶尔先跑完。本机在第 117 次复现。修好后
supervisor 在 `Started` 之后才 spawn，倒序在构造上不可能；测试仍保留 1000 次以防回归（不依赖时间）。
`JobNotifier::spawn` 的订阅顺序（SJ6）在 `radian-host/MUTATIONS.md`。

## review fix 1：`job_output` 读完整输出文件（ADR 0039 决定 1 复核，2026-10-16）

工作树 `oc/sec-jobs` · 变异就地改 `jobs.rs` 的 `FileMode::Tail` 分支，跑完从
`.radian/scratch/mut/jobs.fix1.orig.rs` 还原并 `diff -q` 确认，`git status --short` 干净。
新测试 `cargo test -p radian-coding --test job_output_file`。

新增的 `job_output_file` 三个用例在**修复前**必失败：`tail_lines`/`from_line`/`max_lines`/
`pattern` 不是旧 schema 的属性，`args::parse` 直接报 `unknown argument`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| SJ7 | `job_output` 的 `tail_lines` 只从 64 KiB ring 取最后几行（不读文件） | `job_output_file::the_file_arguments_read_the_full_output_not_the_ring`（`tail_lines: 100000` 的第一行变成 ring 窗口的 `190638`，而不是文件第 `100001` 行） | ✅ failed |

SJ7 说明「tail 从 ring 也能取到小的尾巴」不足以证明读文件：只有尾巴大于 ring 的窗口时才区分得开，
所以用例用 `tail_lines: 100000` 钉住起点行号。`from_line: 1, max_lines: 3` 与
`pattern: ^1234$` 另从 ring 已丢弃的部分各钉一次读文件。
## ADR 0039 decision 2: fd-relative file tools (2026-10-16, branch `sec/fd-file-tools`)

Base: `d1b1dbe` · worktree `.radian-worktrees/sec-fs` · every mutation run with
`cargo test -p radian-coding --no-fail-fast`; `git checkout -- crates/radian-coding/src` after
each; `git status --short` empty at the end. The three mutations the brief asked for, plus two.

| # | Requirement mutated | Mutation | Result |
|---|---|---|---|
| M-A | every parent component opened with `O_NOFOLLOW` | drop `O_NOFOLLOW` from the directory-level `openat` flags in `safe_fs::walk_dirs` | ✅ caught by 10 tests: `every_file_tool_refuses_a_parent_symlink_to_outside_and_touches_nothing_there`, `every_file_tool_refuses_a_parent_symlink_inside_and_names_the_target`, `read_cannot_reach_outside_when_the_parent_is_swapped_after_the_check`, `write_and_patch_cannot_reach_outside_when_the_parent_is_swapped_after_the_check`, `list_glob_and_grep_cannot_reach_outside_when_the_base_is_swapped_after_the_check`, `a_symlinked_parent_directory_escape_is_refused`, `write_through_a_symlinked_parent_creates_nothing_outside`, `write_into_an_outside_symlinked_directory_is_refused`, `a_path_through_a_symlink_outside_is_refused`, `cwd_escapes_are_refused` |
| M-B | no tool re-opens by path after the fd walk | in `read`, after `open_read`, drop the fd and `File::open(root.join(path))` instead | ✅ caught (`read_and_write_use_the_fd_they_opened_even_if_the_parent_is_swapped_afterwards`: the `after_open_hook` seam swaps the parent for a link to the outside sentinel after the open; the mutant reads `SENTINEL-CONTENT`, the real code reads `INSIDE` from the fd). The resolve-hook swap tests do **not** catch this one: they swap before the walk, so the walk itself refuses — which is why the after-open seam exists |
| M-C | the walk never follows a link | classify entries with a following `fstatat(…, 0)` **and** open children / files without `O_NOFOLLOW` | ✅ caught by 15 tests, including `the_walk_skips_linked_directories_and_files_but_keeps_ignore_rules` and `a_symlink_out_of_the_workspace_is_never_followed` |
| M-C1 | (single site) classify entries with a following `fstatat` only | | ✅ caught by 10 tests — not an equivalent mutation: the same `fstatat` is what `map_open_error` uses to tell a link from a missing directory, so the error codes change too |
| M-D | an inside link names its target | make both lexical/realpath classifications return `None` | ✅ caught (`every_file_tool_refuses_a_parent_symlink_inside_and_names_the_target`, `an_absolute_inside_link_target_is_named_relative_to_the_workspace`, `write_refuses_a_symlink_even_inside_the_workspace`, `write_refuses_a_dangling_symlink`) |

What changed for the older rows above: **M7 (`O_NOFOLLOW` on the write open) is now a behaviour
change** — the walk refuses a link before any open, and the after-open seam shows the fd is
what gets written — so the "defence in depth, untestable" note no longer applies. The
parent-directory window the earlier record called "not mitigated at all" is the one M-A/M-B
cover.

## `write` 结果的 `previous_lines`（ADR 0028，2026-10-16）

Base：本片 HEAD（`oc/write-diff`）· 就地变异（改 `write.rs`），每个变异前把文件复制到
`.radian/scratch/mut/write.rs.orig`，跑完用**不带 `-p`** 的 `cp` 还原（mtime 落回当前，避开
`radian-tui/MUTATIONS.md` 记过的「副本旧 mtime → cargo 判定新鲜、变异留在构建缓存」陷阱）并
`cmp` 逐字节确认；结束时 `git status --short` 只剩本片自己的改动。每个变异用
`cargo test -p radian-coding --test mutation --locked --no-fail-fast -- <test>`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| W1 | 覆盖时不再写 `previous_lines`（`if previous_lines > 0` → `if false`） | `write_reports_the_overwritten_files_line_count`、`write_counts_an_old_file_larger_than_max_write_bytes` | ✅ |
| W2 | 先 `truncate` 再数行（读到的旧文件已被清空） | `write_reports_the_overwritten_files_line_count` | ✅ |
| W3 | 行数公式差一（`newlines + 1`，结尾换行也算成一行） | `write_reports_the_overwritten_files_line_count` | ✅ |

3/3 抓住。W1 等同本片之前的行为：旧代码的 `structured` 里根本没有 `previous_lines`，断言
`== 3` 得到 `null` 即失败。空文件不写字面量 0（字段缺席）由同一用例末段钉住；`\r\n` 算一个换行、
结尾换行不算新行由同一用例中段钉住。
## ADR 0039: read-only cargo registry and toolchain roots (2026-10-16, branch `oc/ro-roots`)

Base `5410774` · in-place mutations (each source file backed up under `.radian/scratch/mut/`,
restored from the copy and confirmed byte-identical with `diff -q`; `git status --short` shows
only this slice's own changes at the end). Each mutation run with
`cargo test -p radian-coding --test read_roots --no-fail-fast -- <test> --exact`.

| # | Requirement mutated | Mutation | Result |
|---|---|---|---|
| RR0 | the roots exist at all | `ReadRoots::from_env` returns `Self::none()` | ✅ caught (`a_registry_source_file_is_read_by_absolute_path`: the read is refused) — the "tests fail before the feature" evidence |
| RR1 | `write`/`patch` never accept a read-only root | `open_write` resolves the target with `resolve_read` and opens a `Resolved::Root` target through that root's fd with `WriteCreate` | ✅ caught (`write_to_a_registry_source_file_is_refused_and_leaves_it_alone`: the file is overwritten) |
| RR2 | only `$CARGO_HOME/registry/src`, not all of `$CARGO_HOME` | `cargo_registry_src` returns `cargo_home` (drop `registry/src`) | ✅ caught (`a_file_in_cargo_home_outside_registry_src_is_refused`: `credentials.toml` becomes readable) |
| RR3 | the root walk never follows a symlink | drop `O_NOFOLLOW` from the directory-level `openat` flags in `safe_fs::walk_dirs` | ✅ caught (`a_symlink_inside_the_registry_root_pointing_outside_is_refused`: the link is followed to the outside file) |

4/4 caught, no misses. RR3's mutation is in the shared fd walk, so the workspace tests
(`tests/safe_fs.rs`) catch it too; the named test is the read-roots one. RR0 is recorded because
the brief's first rule is "tests fail on the current branch": with `from_env` disabled the
read-root tests fail on the boundary refusal, not on a missing API.

## orch-s6b（2026-10-16，ADR 0043）：heavy 门的 host 接线

Base 是 `oc/orch-s6b`。变异就地施加，源文件先复制到 `.radian/scratch/mut/`，改完跑一条命名的测试，
再从副本还原并用 `diff -q` 确认字节一致（禁用 `git checkout --`）。每条
`cargo test -p radian-coding --test heavy <test> --no-fail-fast -- --exact`。

| # | 变异的需求 | 变异 | 结果 | 抓住它的测试（rc=101） |
|---|---|---|---|---|
| SC-1 | 排队等槽可以被取消，且取消后不运行命令 | `HeavySlots::acquire` 的 `select!` 里把 `cancel.cancelled()` 分支换成永不就绪的 future（等于忽略取消） | ✅ 7.0s | `cancelling_a_queued_heavy_command_runs_nothing`（取消后仍占住 5s 超时，且标记文件被创建） |
| SC-2 | 非重活命令不占槽、不排队 | `HeavySlots::acquire` 去掉非重活的提前返回（`if !self.is_heavy(command)` 改成 `if false`） | ✅ 6.9s | `a_heavy_command_waits_for_the_slot_and_a_light_one_does_not`（light 命令在槽被占时也等待，5s 超时） |

2/2 抓住。`heavy.rs` 里 `is_heavy` 的前缀匹配（默认模式、边界、`bash -c` 不可见）由同文件的单元测试
`the_default_patterns_match_their_commands_and_only_theirs` 钉住。
## review fix 1：读写打开被拒时回退到只写、跳过行数（2026-10-16）

Base：本片 HEAD（`oc/write-diff`，fix 1 提交前）· 就地变异（改 `workspace.rs`），变异前复制到
`.radian/scratch/mut/workspace.rs.orig`，跑完用**不带 `-p`** 的 `cp` 还原并 `cmp` 逐字节确认；
每个变异用 `cargo test -p radian-coding --test mutation --locked --no-fail-fast -- <test>`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| RF1 | `is_permission_denied` 恒 `false`（去掉回退，即 a035006 的行为：`O_RDWR` 打不开就让整个写失败） | `write_overwrites_a_write_only_file_without_counting_its_lines` | ✅ failed |

同一变异下 `write_reports_the_overwritten_files_line_count` 仍过：回退只影响进程读不了的文件，
普通覆盖不受影响。RF1 就是 a035006 的可观测行为，所以 0200 用例在 a035006 上必失败。0200 用例在
root 下跳过（root 无视 mode 位，构造不出那次拒绝），跳过前先探测能否读一个 0200 文件。

## orch-s6b review fix 1：后台重活 job 在 settle 时释放槽（2026-10-16）

Base `f1be5b1`（本片 HEAD）· 就地变异（改 `jobs.rs` 的 `supervise`），变异前把文件复制到
`.radian/scratch/mut/jobs.rs.orig`，跑完用**不带 `-p`** 的 `cp` 还原并用 `cmp` 逐字节确认；
结束时 `git status --short` 只有本片自己的测试改动。每个变异用
`cargo test -p radian-coding --test heavy --locked --no-fail-fast`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| SC-3 | 删掉 `supervise` 里的 `cell.heavy = None;`（settle 不释放槽） | `a_settled_background_heavy_job_releases_its_slot`、`a_killed_background_heavy_job_releases_its_slot` | ✅ failed |

这一格是评审补的洞：settle 释放槽原先无测试覆盖。已 settle 的 cell 为 `job_list` 留在注册表里，
所以删掉那一行后，结束的后台重活 job 一直攥着槽，直到 cell 被 drop，之后每条重活命令都会永远排队：
两个跑完的 `cargo test` 就能堵死所有后续重活。两个新用例在 settle/`job_kill` 之后断言
`gate.available() == 1`，并让第二条重活命令在 5 s 超时内跑完（变异下断言先失败，不依赖超时）。
`cancel`、非重活不占槽、等待可取消由上一节的 SC-1/SC-2 钉住。
## ADR 0037：前台 shell 调用过阈值转后台（2026-10-16，branch `oc/auto-bg`）

Base：本片 HEAD · 就地变异（改 `shell.rs` / `jobs.rs`），变异前把文件复制到
`.radian/scratch/mut/`，跑完用**不带 `-p`** 的 `cp` 还原并 `cmp` 逐字节确认，结束时
`git status --short` 干净。每个变异用
`cargo test -p radian-coding --test jobs --no-fail-fast -- <test>`（会重新编译）。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| AB1 | 转后台时重启命令：`kill_tree` 掉前台组，再 `jobs.start` 起一个**新** job（新进程） | `a_long_foreground_command_is_adopted_in_place`（`the adopted process (pid …) must be alive`） | ✅ failed |
| AB2 | 丢掉转后台前的输出：`StreamSink::promote` 不把已保留的 head+tail 写进 job 的 `.out` | `a_long_foreground_command_is_adopted_in_place`（输出文件里等不到 `before`） | ✅ failed |
| AB3 | 名额满了仍转：`Jobs::adopt` 的 4 个名额检查关掉 | `four_running_jobs_leave_no_room_and_the_call_is_not_adopted`（`adopted` 不是 `null`） | ✅ failed |
| AB4 | 转后台后 turn 取消仍杀它：`execute` 转后台后 `tokio::spawn` 一个 watcher，`cancel` 时 SIGKILL 进程组 | `an_adopted_job_survives_the_turn_cancellation` | ✅ failed |
| AB5 | 超时在转后台时重置：`remaining = explicit_deadline.saturating_sub(elapsed)` → `= explicit_deadline` | `an_adopted_job_is_killed_by_the_original_timeout`（5.0 s，而非 ~3 s） | ✅ failed |

5/5 抓住。AB1 的 pid 断言是「不重启」的钉子：转后台走的是同一个 `Supervised` 的同一个子进程，
`leader.pid` 不会被覆盖，只写一次；重启变异会覆盖它并杀掉旧 pid，`alive(pid)` 为假。AB2 钉住
「转后台前的输出不丢」。AB4 是「转后台后属于 job、turn 取消不再杀」这条决定的可观测后果，变异
正是一个「忘了把 cancel watcher 摘掉」的实现。

**新用例在旧代码上必失败**（本片前没有 `auto_background_after`）：整条路径与
`Limits.auto_background_after` 在 `025ebfb`（只有 ADR 文本的提交）上不存在，这些用例连字段都
没有；把阈值设回「永不」（`None`）时 `a_long_foreground_command_is_adopted_in_place`
在 60 s 前台默认超时下也永远不会返回 job id。语义上，AB1–AB5 的任一变异等价于把这条行为拿掉，
对应用例即失败（上表）。

### orch-s6b 合并 origin/main（2026-10-16）：转后台的重活命令把 heavy 槽带进 job

`oc/auto-bg` 与 `oc/orch-s6b` 都动 `shell.rs`/`jobs.rs`：前者加了「前台命令过阈值就地转后台」，
后者加了 heavy 槽门。合并时必须两样都留：转后台的重活命令要把手上的 `HeavyPermit` 经
`Adopt`/`Jobs::adopt` 传进 job（和 `Jobs::start` 一样），job settle 时再释放。变异就地改
`crates/radian-coding/src/shell.rs`（adopt 调用处），跑完从 `.radian/scratch/mut/shell.rs.fix2orig`
还原并 `diff -q` 确认字节一致。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| MH-1 | 转后台时不把 heavy permit 带进 job（`.adopt(..., heavy)` 换成 `None`，permit 随前台 `run` 返回即释放） | `an_adopted_foreground_heavy_command_holds_and_releases_its_slot`（转后台后 `gate.available()` 已是 1，断言应为 0） | ✅ failed (rc=101, 4.0s) |

新用例：`an_adopted_foreground_heavy_command_holds_and_releases_its_slot`（`tests/heavy.rs`）——
前台重活命令（`echo heavy adopt; sleep 600`）过 300 ms 阈值转后台后 `gate.available() == 0`，
`job_kill` 使其 settle 后 `== 1`。


## grep: a large `context` is cut, not refused (2026-10-17)

Seven dogfood calls failed schema validation on `context` 12-30 against `maximum: 10`, each a wasted model round trip. Now the limit is 30 and a larger ask is cut with a `[context cut to 30 lines]` line. Run: `cargo test -p radian-coding --test search grep_`.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| GC-1 | the schema keeps `"maximum": 10` (the old schema) | caught (rc=101) | `grep_schema_does_not_cap_context_below_the_cut` |
| GC-2 | the cut is silent (no `[context cut ...]` line) | caught (rc=101) | `grep_caps_the_context_argument` |

## job_output: `mode` names the read (choice A)

GPT fills every optional field (`tail_lines: 1`, `from_line: 1`, `max_lines: 300`, ...), which the "at most one read" rule refused six times in one lead session. An optional `mode` (`new` / `tail` / `range` / `pattern`) now names the read and the other reads' fields are ignored; without `mode` the old rule holds. Run: `cargo test -p radian-coding --test job_output_file`.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| JM-1 | `mode` is ignored (`match None::<&str>`) | caught (rc=101) | `mode_names_the_read_and_ignores_the_other_fields` |
| JM-2 | `mode: tail` reads the range instead | caught (rc=101) | `mode_names_the_read_and_ignores_the_other_fields` (only the last line) |

A third mutation (count a blank `pattern` as a read) survived: `args::parse` already drops an optional `""` (args.rs:58-69), so the extra filter was redundant and was removed; the test pins the parse rule for GPT's calls.

## a stop signals the job's whole session, not only its process group (2026-10-17)

A child that calls `setpgid` — GNU `timeout` does, and so does a shell's job control — leaves the job's process group but **keeps its session**, so the group-only kill left it running with PPID 1 (dogfood: the lead's `timeout -k 30 2400 cargo test` outlived the 600 s job kill; a `mcp-fixture` survived a group kill the same way). `shell::signal_session` now signals the session: the group first, then every process whose session is the job's (`proc_listallpids` + `getsid` on macOS, `/proc/<pid>/stat` field 6 on Linux, unverified).

Five tests in `tests/job_session_kill.rs`, one per stop path (`job_kill`, `timeout_secs`, session-end `kill_all`, the leader's own natural exit, a foreground timeout). The escaping child is built by the test itself — `perl -e 'use POSIX; setpgid(0,0); exec @ARGV' sleep 600` — because GNU `timeout` is coreutils (a homebrew install), not a macOS tool; a missing `perl` fails the test with a clear message, never skips. Each test asserts the shape **first** — the child is in its own process group but in the job's session — then that the child is gone and the session is empty; a child that never left the group would make the test vacuous. Against the pre-fix code (8845f383) all five fail on the direct assertion (0 passed; 5 failed).

Run: `cargo test -p radian-coding --test job_session_kill --no-fail-fast` (only this binary; mutations in `src/jobs.rs` / `src/shell.rs`, restored from `.radian/scratch/mut/*.orig` and compared byte-for-byte). Note: every run above went through `cargo test`, which relinks the executable. Running `target/debug/deps/job_session_kill-*` **directly** right after restoring a source file runs the *previous* build — `cargo clippy` does not relink it — which looks exactly like five new failures (measured: 0 passed, 24 s per run, the pre-fix timing). Relink with `cargo test … --no-run` first if a direct run is ever needed.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| SK-1 | the enumeration finds nothing (macOS `session_members` returns `Vec::new()` at once) | caught (rc=101, 0/5) | all five: only the group signal is left |
| SK-2 | the supervisor's own kill stays group-only (`supervise`: `signal_session` -> `signal_group`) | caught (rc=101, 4/5) | `a_natural_exit_reaches_a_child_that_left_the_process_group` |
| SK-3 | the foreground kill stays group-only (`kill_group` -> `signal_group`) | caught (rc=101, 4/5) | `a_foreground_timeout_reaches_a_child_that_left_the_process_group` |
| SK-4 | `signal_if_reserved` stays group-only (`kill_all` and `terminate`), **alone** | survives: masked by the session SIGKILL that follows, end state identical; kept as a redundant first step | none — see below |
| SK-5 | SK-2 and SK-4 together (every `jobs.rs` signal group-only) | caught (rc=101, 4/5) | the four background paths (the foreground path is a different helper) |

SK-4 survives because it is masked: `job_kill`/the timeout/`kill_all` still kill the leader (its group is the leader's), and once the leader dies the supervisor reaps it and kills the whole session (the SK-2 line), so the child that left the group dies anyway. The end state is identical; what SK-4 alone loses is the SIGTERM an escaped child would get during the grace, which no test in this suite can time (the supervisor's SIGKILL follows the leader's death by microseconds). SK-5 pins that reading: with the supervisor's line mutated too, the four background tests fail, so the session signal in `signal_if_reserved` is carried by `supervise` on this path. It is kept as a redundant first step: it is what the ADR 0037 decisions 4 and 6 state (SIGTERM to the whole session, then SIGKILL after the grace; see the ADR's 实施补记 of 2026-10-17), and it does not depend on the leader dying first.

The reuse guard has no test either, before or after this change: every signal is still sent under the leader's `reaped` lock while the leader is unreaped, and no test can force the pid reuse it guards against.

## ADR 0043 slice ③: the tool merge and the deleted heavy gate (2026-10-17)

`git_status` became `git_diff { stat: true }` (with the old name as a one-version alias) and the
heavy-command gate (`src/heavy.rs`, its `jobs.rs` permit plumbing and the `shell.rs` queueing) is
deleted, so shell commands no longer queue. The mutations below were applied in place, each
restored from a `cp` in `.radian/scratch/mut/` and confirmed byte-identical with `cmp` before the
next one (no `git checkout --`/`restore`/`stash`); `git status --short` was checked after each run.

Run: `cargo test -p radian-coding --test shell_overlap --locked --no-fail-fast`
(`shell_overlap` runs one test in its own binary, so its `PATH` change reaches no other test).

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| HM-1 | the one-slot heavy gate is back in `shell::run` (a process `Semaphore::new(1)` acquired for a command starting with `cargo test`) | ✅ 1 failed (7.1 s: the first stub's bounded barrier wait expired, `exit code: 3`, `barrier timed out waiting for …/second.started and …/third.started`) | `three_heavy_looking_shell_commands_run_at_the_same_time` |

HM-1 is the required "put the heavy gate back with 1 slot" mutation: it fails on the barrier, not
on a timeout of the test, because the stub's wait is bounded and the test asserts the proof line
`all three ran` (only three-way overlap produces it).

**The old code, not just the 1-slot mutation** (fix round 1): the same test file was copied into a
scratch extraction of 29f0387 (`git archive 29f0387 | tar -x -C /tmp/s3-old-29f0387`, its own
`CARGO_TARGET_DIR`; the worktree's committed state was not touched) and run there with the real
heavy gate (`heavy.rs:39` `DEFAULT_HEAVY_SLOTS = 2`, `shell.rs:276` `heavy.acquire`):

```
cargo test -p radian-coding --test shell_overlap --no-fail-fast   # in /tmp/s3-old-29f0387
test result: FAILED. 0 passed; 1 failed  (4.4 s)
  the second command … [queued for a heavy slot: waited 0 ms before running]  exit code: 3
  barrier timed out waiting for …/third.started and …/first.started
```

Two stubs would **not** have caught this: the old gate's default is two slots, so a two-command
version of this test passes on 29f0387 (the reason this row now runs three).

## ADR 0037: an explicit background `timeout_secs` is ignored (2026-10-17)

`shell::start_background` no longer forwards `timeout_secs` to the job registry, so a
`background: true` command has no deadline; the schema and the acknowledgement say so. This is the
one-directional check the revision asks for (restore the deadline). The mutation was applied in
place, restored from a `cp` in `.radian/scratch/mut/` and confirmed byte-identical with `cmp`
before the next step (no `git checkout --`/`restore`/`stash`); `git status --short` was checked
after each run.

Run: `cargo test -p radian-coding --test jobs --locked --no-fail-fast a_background_timeout_secs_is_ignored`

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| BG-1 | `start_background` passes the deadline again (`jobs.start(…, args.timeout_secs.map(Duration::from_secs))`) | ✅ 1 failed (1.0 s: the job settled at the restored 1 s deadline, so `job_output wait` returned `[status: timed out]` where the test asserts `[status: running]`) | `a_background_timeout_secs_is_ignored` |

BG-1 is also the "old code" failure the brief asks for: the pre-revision code passed the deadline
unconditionally, so the same test fails on it for the same reason. The other half of the revision
(`a_foreground_timeout_secs_still_kills_the_command`) is the unchanged foreground path; the
revision-1 adoption-deadline test (`an_adopted_job_is_killed_by_the_original_timeout`) is green
without change.

## ADR 0037: a `job_output wait` on an already-settled job waited out its timeout (2026-10-17)

`settle` used `cell.settled.send(true)`, and tokio's `Sender::send` returns `Err` **without
storing** the value when `receiver_count() == 0`; `start`/`adopt` drop the receiver from
`watch::channel(false)` at once, so a job that settled with no waiter left the watched value
`false` and a later `output(wait: true)` slept to its timeout (observed in a lead session: 300 s,
with `[still running after 300000 ms; the job was not killed]` and `[status: exited 0]` in one
text). The fix: `send_replace(true)` in `settle`, a pre-wait state check in `output` (a settled
job never waits), and the `[still running ...]` line only while the state says running.

Mutations were applied in place, restored from a `cp` in `.radian/scratch/mut/` and confirmed
byte-identical with `cmp` before the next step (no `git checkout --`/`restore`/`stash`);
`git status --short` was checked after each run.

Runs:
`cargo test -p radian-coding --test jobs --locked --no-fail-fast settled` and
`cargo test -p radian-coding --lib --locked --no-fail-fast jobs::`

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| JW-1 | `settle`: `send_replace(true)` → `send(true)`, pre-wait state check kept | ❌ lib 1 failed / ✅ integration 2 passed in 0.31 s | `a_settle_with_no_waiter_still_stores_the_watched_value` (white-box) — the two integration tests never reach the watch, because the pre-wait state check returns first |
| JW-2 | pre-wait state check dropped, `send_replace` kept | ✅ 2 passed in 0.31 s — **not caught alone** | none (belt and braces: `send_replace` alone already makes a late subscriber see `true`) |
| JW-3 | `[still running ...]` printed whenever the wait timed out (parts 1+2 kept) | ✅ 2 passed in 0.31 s — **not caught alone** | none (parts 1+2 make `waited_out` unreachable on a settled job; the raced case is not deterministic) |
| JW-4 | the pre-fix code for parts 1+2 (`send`, no state check) | ❌ 2 failed in 30.31 s | `a_wait_on_a_settled_job_returns_at_once` (`must return at once, not after its timeout: 30.0027215s`), `a_wait_on_a_settled_adopted_job_returns_at_once` (`30.002252709s`) |
| JW-5 | the pre-fix code entirely (`send` + no state check + unconditional `[still running ...]`) | ❌ 2 failed in 30.31 s, on the message assertion first | both new tests: `a settled job must never be reported as still running: done` / `[still running after 30000 ms; the job was not killed]` / `[status: exited 0]` |

JW-4 is the "old code" failure the brief asks for: the new tests wait the full 30 s `timeout_ms`
and fail the `< 2 s` bound (measured 30.0027 s and 30.0023 s). JW-5 is the observed bug verbatim:
the same 30 s wait whose text carries both `[still running after 30000 ms]` and
`[status: exited 0]`.

The white-box test `a_settle_with_no_waiter_still_stores_the_watched_value` is what catches the
`send` mutation in `settle` alone (JW-1); behaviourally the two integration tests only fail when
the pre-wait state check is absent too (JW-4). The pre-wait state check (JW-2) and the message
guard (JW-3) are each unreachable alone and no deterministic test catches either: they are belt
and braces over `send_replace`, recorded honestly.

## `patch`: atomic multiple edits (`edits`) (2026-10-17, branch `pi/multi-edit-output`)

The new form locates every edit against the **original** file before anything is written, rejects
overlapping/identical ranges, checks the combined size, and only then does the one existing
truncate/write. Mutations were applied in place, the source restored from a `cp` in
`/tmp/radian-mut/` and confirmed byte-identical with `cmp` after each run; run with
`cargo test -p radian-coding --test patch --locked --no-fail-fast`.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| ME-1 | write the first edit as it resolves, before the later edits are validated (`run` truncates/writes edit 0 inside the resolve loop) | ✅ 1 failed | `a_missing_second_edit_leaves_the_first_unapplied` (`left: "ALPHA beta\n"` — the first edit had been written although the second was missing) |
| ME-2 | skip the overlap check (`let _ = check_conflicts(&args.path, &resolved);`) | ✅ 2 failed | `overlapping_ranges_are_refused`, `identical_ranges_are_refused` (both returned `Ok` and wrote the file) |

ME-1 is the "apply the first edit before validating the second" mutation the brief names; the test
fails on the file's contents, not on the error code, which is exactly the all-or-nothing property
under test. ME-2 shows the conflict rejection is the only thing guarding overlapping ranges:
without it the two ranges are applied in descending offset order and the file changes silently.

## `shell_output`: a foreground shell result's opaque, bounded output handle (2026-10-17, branch `pi/multi-edit-output`)

A capped foreground `shell` command tees its whole output to a session-private file (the shared
`output::open_output_file` creator, `O_CREAT|O_EXCL|O_NOFOLLOW` 0600) and returns an opaque handle
`out-<nonce>-<n>`. `shell_output` resolves the handle only in the session's own registry; a handle
is never a path, and the retained file is capped per item and per session. Mutations were applied
in place, the source restored from a `cp` in `/tmp/radian-mut/` and confirmed byte-identical with
`cmp` after each run; run with `cargo test -p radian-coding --test shell_output --locked
--no-fail-fast`.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| SF-1 | `run_read` resolves the handle directly as a path (`PathBuf::from(&args.handle)`), bypassing the registry | ✅ 4 failed | `a_path_like_handle_cannot_open_a_file`, `a_handle_from_another_session_is_unknown`, `a_truncated_command_exposes_a_retrievable_handle`, `the_session_cap_evicts_the_oldest_handle` (the path read gave `output_read` instead of `unknown_output`) |
| SF-2 | `Retention::push` ignores the per-item cap (`take = bytes.len()`) | ✅ 1 failed | `the_retained_file_is_capped_and_says_so` (`output_retained_cut` was `false`, retained 23893 > 200) |
| SF-3 | `ForegroundOutputs::finish` never takes the "remove it" branch (`if !keep` → `if false`) | ✅ 1 failed | `a_non_truncated_command_leaves_no_handle_or_file` (one leftover `.out` file for an untruncated command) |

SF-1 is the required "bypass the opaque-handle registry with a path/open" mutation; the path test
never opens the planted file. SF-2 is the required "omit the cap/retention enforcement" mutation.
SF-3 pins the "a non-truncated command leaves no artifact" rule: without it the file survives even
though no handle was returned.

The **shared creator** (`output::open_output_file`, used by both `job_output`'s file and
`shell_output`'s file) is pinned by the unit test `output::tests::the_creator_refuses_an_existing_file_and_a_symlink`
and by `job_output_security::a_planted_symlink_at_the_next_output_file_is_not_followed`: a planted
symlink or pre-existing file at the destination is refused, so it can never receive host output.

## review fix: the no-host foreground root and the retention cleanup (2026-10-17, branch `pi/multi-edit-output`)

The review found the no-host `CodingTools::with_sandbox` path retaining foreground output under the
workspace (readable by the model's `read`), and an error path leaking the retention file.
Foreground retention is now enabled **only** when the provider has a caller-supplied private
directory (`with_jobs`); the private root is held as an `O_DIRECTORY|O_NOFOLLOW` fd and every file
is created/read relative to it; and a `Drop` guard removes the file on any path that did not commit
it. Mutations were applied in place, restored from a `cp` in `/tmp/radian-mut/rf/` and confirmed
byte-identical with `cmp`; run with
`cargo test -p radian-coding --test shell_output --locked --no-fail-fast <test>`.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| RF-1 | the no-host branch retains in the workspace fallback again (`ForegroundOutputs::new` over `jobs.dir()`) | ✅ 1 failed | `the_no_host_provider_retains_nothing_in_the_workspace` (`shell_output` was advertised and a file landed under the workspace) |
| RF-2 | `RetentionInner::drop` no longer removes the file | ✅ 1 failed | `a_spawn_failure_leaves_no_retained_file` (`1` leftover `.out` file) |
| RF-3 | `PrivateDir::open` follows a symlinked root (drop the symlink check and `O_NOFOLLOW`) | ✅ 1 failed | `a_planted_root_symlink_cannot_redirect_output` (a handle appeared; output was written through the link) |

RF-2 **survived its first run**: the test's jobs parent did not exist, so `PrivateDir::open`
failed, no file was ever created, and the assertion passed vacuously. The test now creates the
session directory tree first, and the mutation then fails on the leftover file. The record keeps
this because it is the honest picture: an error-cleanup test must prove the file existed first.

## image inputs: `read`, `@`, dropped path, `exec --image` (2026-10-22, branch `work/TK0013-image-inputs`)

One pipeline (`src/image.rs`) does magic number → limits → part. Backups were `cp`ed under
`.radian/` and restored byte-identically.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| IMG-1 | the kind comes from the extension instead of the magic number (`load` matches `name.rsplit('.')`) | ✅ 1 failed | `radian-coding::images::the_extension_never_decides_the_kind` (a text `.png` became an image, a PNG `.txt` did not) |
| IMG-2 | the image part is sent even when `ImageSupport` says the model takes none (`if true`) | ✅ 1 failed | `radian-coding::images::a_model_known_blind_gets_only_the_text_and_the_omission_line` (two parts instead of one) |
| IMG-3 | the 5 MiB byte limit is dropped (`MAX_IMAGE_BYTES = u64::MAX`) | ✅ 1 failed | `radian-coding::images::the_byte_and_pixel_limits_are_image_too_large` and `image::tests::the_limits_are_named_in_the_error` |
## `write` creates a missing parent; `shell` clamp notes move (2026-10-23, branch `work/TK0023-write-parents-shell-clamp`)

Base: the branch's ADR-only commit. Mutations were applied in place, each source file backed up
under `.radian/mut/<letter>/` and restored from that copy, confirmed byte-identical with `cmp`;
run with `cargo nextest run -p radian-coding --locked --profile sandbox --no-fail-fast -E '<tests>'`.
New tests first ran against the unmodified source: `write_creates_missing_parent_directories_and_patch_does_not`
failed with `not_found`, `the_write_spec_says_parents_are_created` failed on the old "parent
directory must exist" text, and both clamp tests failed on the old text/position. The link-parent
test and `a_timed_out_result_keeps_its_marker_before_exit_code` pass on the old source too: they
are guards for the change (a mutation below proves each is live), not evidence of the feature.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| WP-1 | create the missing parents by path: `std::fs::create_dir_all(root.join(parents))` before the fd walk (the per-level variant is **not** a catching mutation: the walk refuses the link level first, so its `mkdirat` replacement never runs) | ✅ caught (`write_creates_no_level_under_a_symlinked_parent`: `create_dir_all` followed the link and created `outside/a`, so the outside directory held two entries) | `write_creates_no_level_under_a_symlinked_parent` |
| WP-2 | do not refuse a link level: drop `O_NOFOLLOW` from the directory-level `openat` flags in `safe_fs::walk_dirs` | ✅ caught by both (`write_creates_no_level_under_a_symlinked_parent` wrote through the link; `every_file_tool_refuses_a_parent_symlink_to_outside_and_touches_nothing_there` returned `Ok` instead of the boundary refusal) | `write_creates_no_level_under_a_symlinked_parent`, `every_file_tool_refuses_a_parent_symlink_to_outside_and_touches_nothing_there` |
| SC-1 | leave the timeout clamp note after the stderr section (emit it after `[output was not fully drained …]` instead of before `exit code:`) | ✅ caught (`a_clamped_timeout_is_a_marker_before_exit_code_with_both_numbers`: the marker was found only after `--- stderr ---`) | `a_clamped_timeout_is_a_marker_before_exit_code_with_both_numbers` |

The `write` swap test (`write_and_patch_cannot_reach_outside_when_the_parent_is_swapped_after_the_check`)
stays green under the real code: the missing-level creation never runs before the walk refuses the
swapped link level.

## Test temp directories are removed when their test ends (2026-10-23, branch `work/TK0041-tests-delete-temp-dirs`)

Base: the branch's code commit. Files were backed up under `.radian/` and restored from that
copy, not with `git checkout`; the leak test is
`crates/radian-coding/tests/temp_leak.rs::every_test_temp_directory_is_removed_when_its_test_ends`.
It runs one test from each large family in its own child process and only counts entries named
with that child's pid (nextest runs tests in parallel). Run with
`cargo nextest run --workspace --locked --profile sandbox -E 'test(=every_test_temp_directory_is_removed_when_its_test_ends)'`.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0041-M1 | put the `radian-coding-tests-<pid>` shared root back in `tests/common/mod.rs`: create it, put each scratch dir under it, and keep removing only the child | ✅ caught: `the child tests left temp entries behind: ["radian-coding-tests-66773"]` | `every_test_temp_directory_is_removed_when_its_test_ends` |
| TK0041-M2 | a child-process test writes its sessions outside its temp directory again: `radian-exec`'s `tests/subagent.rs` `Env::new` points `RADIAN_SESSION_DIR` at `$TMPDIR/radian-exec-sessions-<pid>` instead of `<home>/sessions` | ✅ caught: `the child tests left temp entries behind: ["radian-exec-sessions-79422"]` | `every_test_temp_directory_is_removed_when_its_test_ends` |

M2's first attempt changed only the `spawn_parent_env` variable and kept `Env::sessions` under
`<home>`; the exec test itself then failed (`child_journals` found no journal), which fails the
leak test for the wrong reason. The recorded mutation moves the directory the test really reads
and writes, so the child test passes and only the leftover fails the leak test.

## the temp file system check of `verify.sh` and `arc-gate.sh` (2026-10-23, branch `work/TK0042-verify-inode-check`)

Base: the branch's first commit. Mutations were applied by hand to `scripts/temp-check.sh`, backed
up under `.radian/temp-check.sh.bak` and restored from that copy (never `git checkout`); run with
`cargo nextest run -p radian-coding --locked --profile sandbox --no-fail-fast --test temp_check`.
The new tests first ran against the source without `scripts/temp-check.sh`: all nine failed (the
helper did not exist).

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0042-M1 | compare with `-gt` where the inode check means `-lt` (`inode_free -gt inode_limit`) | ✅ 5 failed | `low_inodes_fail_with_the_line` (10 free passed the check), also `plenty_free_passes`, `macos_and_gnu_inode_output_both_parse`, and both override tests |
| TK0042-M2 | ignore `RADIAN_VERIFY_MIN_FREE_INODES` (`local inode_limit="50000"`) | ✅ 2 failed | `the_inode_limit_can_be_overridden_and_zero_turns_it_off` (the `0` and lowered-limit runs failed) and `a_non_numeric_limit_fails_the_check` |

## the temp file system check's follow-ups (2026-10-24, branch `work/TK0049-verify-inode-check-followups`)

Base: the branch's first commit. Mutations were applied by hand to
`scripts/temp-check.sh`, backed up under `.radian/` and restored from that copy (never
`git checkout`); each named test ran alone with
`cargo nextest run -p radian-coding --locked --no-fail-fast --test temp_check <test>`.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0049-M1 | validate only the inode total, not `inode_free` (drop `|| ! radian_temp_is_count "$inode_free"`) | ✅ 1 failed | `macos_and_gnu_inode_output_both_parse` (the `-` ifree printed `[: -: integer expression expected` and the check passed) |
| TK0049-M2 | take the leading `-` of the space column for an unreadable value (drop the `-*` case) | ✅ 1 failed | `a_negative_free_space_fails_the_space_check` (`Some(0)` with "cannot read the free space", not `Some(1)`) |
| TK0049-M3 | do not bound a limit (remove the `-gt 18` check in `radian_temp_limit`) | ✅ 1 failed | `a_limit_past_the_bound_fails_the_check` (`[: 9999999999999999999: integer expression expected`, exit `0`) |

## the user manual's `arc/` pages (2026-10-24, branch `work/TK0089-arc-docs-subdir`)

Base: the branch's move commit. Each mutation was applied by hand to
`crates/radian-coding/src/docs.rs`, backed up under `.radian/docs.rs.bak` and restored from
that copy (never `git checkout`), and the named test ran alone with
`cargo nextest run -p <crate> --locked --profile sandbox -E 'test(<name>)' --no-fail-fast`.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0089-M1 | `lookup_topic` drops the directory fallback (`find_entry(&name)` only), so `radian docs arc` looks for `arc.md` | ✅ 1 failed | `radian-tui::docs_sync::radian_docs_reads_directory_topics_and_nested_pages` (`arc opens its index`) |
| TK0089-M3 | `DOCS` drops the `arc/gate.md` row | ✅ 1 failed | `radian-coding::docs::the_table_matches_docs_user_on_disk` (disk 28 pages vs table 27) |

2/2 caught.

## `read` of an image ignores `offset` and `limit` (2026-10-24, branch `work/TK0118-read-image-ignores-range`)

Base: the branch's code commit. The mutation was applied by hand to
`crates/radian-coding/src/read.rs`, backed up under `.radian/read.rs.orig` and restored from
that copy (never `git checkout`), confirmed byte-identical with `diff -q`; the named test
ran alone with
`cargo nextest run -p radian-coding --locked --profile sandbox --test images -E 'test(<name>)'`.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0118-M1 | return the old refusal again (`if offset_given || limit_given` → `image_offset`) | ✅ 1 failed | `offset_and_limit_on_an_image_are_ignored_and_said_once` (the `offset`+`limit` call failed with `image_offset`) |

1/1 caught.

## a composer attachment name's control characters (2026-10-25, branch `work/TK0123-composer-attachment-align-followups`)

A dropped image's source name is a file name, and a file system allows a control character in
one. `[image: <name>, W×H, N KB]` is one display row (ADR 0028 R5), so `image::placeholder` shows
each control character as a space. The mutation was applied by hand to
`crates/radian-coding/src/image.rs`, backed up under `.radian/image.rs.orig` and restored from
that copy (never `git checkout`), confirmed byte-identical with `cmp`; each named test ran alone
with `cargo nextest run -p <crate> --locked --profile sandbox`.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0123-M1 | `placeholder` formats the source name unchanged (drop the control-character → space map) | ✅ 2 failed | `radian-coding image::tests::a_control_character_in_the_source_name_stays_on_one_line` (`"[image: bad\nname\r\t.png, 10×10, 1 KB]"`); `radian-tui::render a_composer_attachment_name_with_a_line_break_stays_on_one_row` (`Lines: the control characters became spaces`) |

1/1 caught.

## foreground timeout/cancel SIGTERM grace (2026-10-25, branch `work/TK0174-foreground-timeout-sigterm-grace`)

The foreground stop path now SIGTERMs the session, waits up to the shared `KILL_GRACE` (3 s) for the leader to exit unreaped, then SIGKILLs what is left (ADR 0037 §7). The mutation sent SIGKILL first again, as the old code did. It was applied by hand to `crates/radian-coding/src/shell.rs`, backed up under `.radian/shell.rs.orig` and restored from that copy (never `git checkout`), confirmed byte-identical with `diff -q`; the four tests ran with `cargo nextest run -p radian-coding --locked --profile sandbox --test shell -E 'test(a_timeout_sigterms_before_it_kills) + test(a_timeout_kills_after_the_grace_when_sigterm_is_ignored) + test(a_cancellation_sigterms_before_it_kills) + test(a_cancellation_kills_after_the_grace_when_sigterm_is_ignored)'`.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0174-M1 | `Supervised::stop` sends SIGKILL at once instead of SIGTERM (`signal_if_reserved(self.pid, SIGKILL, &reaped)`) | ✅ 4 failed | `a_timeout_sigterms_before_it_kills` (exit code null, signal 9, no `trapped.marker`); `a_timeout_kills_after_the_grace_when_sigterm_is_ignored` (SIGKILL at 1.03 s); `a_cancellation_sigterms_before_it_kills`; `a_cancellation_kills_after_the_grace_when_sigterm_is_ignored` |

1/1 caught.

## an absolute path inside the workspace (2026-10-26, branch `work/TK0176-absolute-path-inside-workspace`)

The rule (ADR 0039 decision 6): an absolute path under the workspace root, in either spelling
(the given one and the canonical one), is the workspace-relative remainder and takes exactly
the checks a relative path gets. Each mutation was applied by hand to
`crates/radian-coding/src/workspace.rs`, backed up under `.radian/workspace.rs.orig` and
restored from that copy (never `git checkout`), confirmed byte-identical with `shasum -a 256`;
`cargo nextest run -p radian-coding --locked --profile sandbox --no-fail-fast` ran the named
binaries (`--test absolute_inside`, plus `--test escaping` for M1). This change inverted the
old `escaping::an_absolute_path_inside_the_workspace_is_still_refused`, so the earlier rows
that name it describe the rule before this task.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0176-M1 | compare string bytes instead of path components (`text.strip_prefix(prefix)` in `workspace_remainder`) | ✅ 2 failed | `a_sibling_that_only_shares_a_string_prefix_is_not_under_the_root` (`/…/ws2/secret.txt` was rewritten to `2/secret.txt` and read instead of refused); also `escaping::a_sibling_whose_name_extends_the_root_is_refused` |
| TK0176-M2 | convert only the given spelling, not the canonical one (`self.given.as_ref().unwrap_or(&self.root)`) | ✅ 4 failed | `every_file_tool_takes_an_absolute_path_under_the_root_in_both_spellings` (the canonical round was refused); `the_root_itself_as_an_absolute_path_lists_and_searches_the_workspace`; `an_absolute_workspace_path_still_refuses_a_symlink_component` (the canonical spelling took the outside route and read the sentinel); `a_deny_set_file_inside_the_workspace_behaves_as_its_relative_path` |
| TK0176-M3 | drop the workspace-remainder route in `resolve_read`, so an absolute path under the root takes the `full`-tier outside route | ✅ 4 failed | `a_deny_set_file_inside_the_workspace_behaves_as_its_relative_path` (refused as denied, while the relative spelling reads); `an_absolute_workspace_path_still_refuses_a_symlink_component` (the full-tier round returned `SENTINEL-CONTENT` through `realpath`); `every_file_tool_takes_an_absolute_path_under_the_root_in_both_spellings` (the workspace tier refused the path); `the_root_itself_as_an_absolute_path_lists_and_searches_the_workspace` |

3/3 caught.
## a stop reaches descendants that started their own session (2026-10-26, branch `work/TK0175-stop-descendant-tree`)

A stop now collects the leader's descendant tree across sessions (`Descendants` in
`crates/radian-coding/src/shell.rs`) and signals it with the session; the grace paths pass the
set their SIGTERM collected to their SIGKILL pass. Each mutation was applied by hand to
`crates/radian-coding/src/shell.rs`, backed up under `.radian/shell.rs.orig` and then undone
either from that copy (M1, M2) or by reverting the one edited line (M3, so the test improvements
made in the same file after the backup stayed), never with `git checkout`; the file was
confirmed by `diff` to hold only the intended change. The integration tests ran with
`cargo nextest run -p radian-coding --locked --profile sandbox --test job_setsid_child` and the
unit test with `cargo nextest run -p radian-coding --locked --profile sandbox --lib -E
'test(a_collected_pid_with_a_changed_start_time_is_not_signalled)'`.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0175-M1 | `Supervised::stop` signals only the session (`signal_session(self.pid, SIGTERM)`, an empty tree for the SIGKILL pass): no walk on the foreground timeout/cancel path | ✅ 2 failed | `a_foreground_timeout_reaches_a_child_that_started_its_own_session` (the `sleep` child survived the timeout); `a_foreground_timeout_reaches_an_ignoring_child_after_its_parent_died` (the child outlived the SIGKILL pass) |
| TK0175-M2 | the SIGKILL pass starts from an empty set (`let mut tree = Descendants::default()`), so it has only a fresh walk, not the set collected at SIGTERM | ✅ 2 failed | `a_foreground_timeout_reaches_an_ignoring_child_after_its_parent_died`; `job_kill_reaches_a_child_that_started_its_own_session` (the ignoring child, a child of pid 1 by the SIGKILL pass, survived) |
| TK0175-M3 | `Descendants::signal` drops the start-time check (`if entry.in_session { continue; }`) | ✅ 1 failed | `shell::tests::a_collected_pid_with_a_changed_start_time_is_not_signalled` (`a pid whose start time changed must not be signalled`) |

3/3 caught.

## a stop's pid list is sized in pids (2026-10-26, branch `work/TK0175-stop-descendant-tree`, review fix 1)

`proc_listallpids(NULL, 0)` answers a **pid count**, and the tree walk's enumeration read that
answer as a **byte count**, so on a machine with more than about 340 processes the pid list was
truncated — and a leader or its `setsid` descendant missing from the list is never walked and
survives the stop. The fix sizes the buffer in `pid_t` slots and doubles it when a fill uses every
slot; `all_processes` and `session_members` now share that one enumeration. The mutation put the
reviewed bug back (slots from `needed / size_of::<pid_t>() + HEADROOM` and the first fill kept, no
retry). It was applied by hand to `crates/radian-coding/src/shell.rs`, backed up under
`.radian/shell.rs.orig`, and restored by reverting the two edited spots (never `git checkout`),
confirmed with `git diff` to hold only the intended change. The test ran with
`cargo nextest run -p radian-coding --locked --profile sandbox --lib -E
'test(the_pid_list_is_not_truncated)'`.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0175-M4 | `all_pids` sizes its buffer from the size query as bytes (`needed / size_of::<pid_t>() + HEADROOM`) and keeps the first fill (no retry) | ✅ 1 failed | `shell::tests::the_pid_list_is_not_truncated` (`the kernel reports 953 pids, the enumeration found 302`) |

1/1 caught.

## an absolute path inside the workspace: the `..` refusal comes before any route (2026-10-26, branch `work/TK0176-absolute-path-inside-workspace`, review fix 1)

The review found that the `full` tier normalised an absolute path holding `..` after declining the
workspace conversion, so `read`/`grep`/`glob` on `<workspace>/sub/../a.txt`, or on
`<workspace>/../outside/sentinel.txt`, took the outside route and read the target. `resolve_read`
now refuses an absolute path with a `ParentDir` component in every tier, before the read-only-root
lookup and the `full`-tier outside route (`has_parent_dir` is shared with `workspace_remainder`, so
the conversion and the refusal agree on what `..` is). The mutation deleted that refusal, putting
the reviewed flow back. It was applied by hand to `crates/radian-coding/src/workspace.rs`, backed
up under `.radian/workspace.rs.rework.orig` and restored from that copy (never `git checkout`),
confirmed byte-identical with `shasum -a 256`.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0176-M4 | `resolve_read` drops the early absolute-`..` refusal, so the path reaches `normalise_absolute` and the `full`-tier outside route | ✅ 1 failed | `absolute_inside::an_absolute_path_with_a_parent_component_is_refused_in_the_full_tier` (`read` on `<workspace>/sub/../a.txt` returned `1: INSIDE` instead of refusing) |

1/1 caught.

## TK0216: a tool description names only mounted tools (2026-10-27, branch `work/TK0216-shell-description-follows-tool-set`)

A restricted `--tools` set must not leave a mounted tool's text naming an absent one (ADR 0043 §4).
`ToolPresence` is built from the allow-list at `HostTools::open_session`, and `shell`/`read` build
their text from it; the full set passes `ToolPresence::ALL` and stays byte-identical. Each mutation
was applied by hand, the named test binary was run, and the source was restored from a copy under
`.radian/` (`git diff` empty).

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0216-M1 | `shell::description` always adds the `read` advice (`if true` instead of `if presence.read`) | ✅ 1 failed (rc=100) | `shell_alone_keeps_only_its_own_rules` (`"`read`" must be absent when only `shell` is mounted`) |
| TK0216-M2 | both search sentences are always appended (the `presence.grep`/`presence.glob` match forced to `(true, true)`) | ✅ 1 failed (rc=100) | `a_read_write_patch_shell_set_names_no_search_or_job_tool` (`` `grep` `` appears in the four-tool description) |
| TK0216-M3 | `shell::spec` always passes `job = true` to the schema, so the `background` field keeps "Read the job with `job`." | ✅ 1 failed (rc=100) | `a_read_write_patch_shell_set_names_no_search_or_job_tool` (the background note names `job`) |
| TK0216-M4 | `read::spec` always adds "Remove it before you use the text in `patch`." (`if true`) | ✅ 1 failed (rc=100) | `read_names_patch_only_when_patch_is_mounted` (the `read,glob,grep` set still names `patch`) |

## TK0217: tool-description follow-ups (2026-10-27, branch `work/TK0217-tool-descriptions-match-tool-set-followups`)

The review of TK0216 found that `job` still said "A `read` with no range", which reads as the
file tool in a `shell,job` set that mounts no `read` (ADR 0043 §4). The description now says
"The `read` op", so the word is the operation the tool accepts. The mutation put the old
wording back; the source was restored from `.radian/jobs.rs.rework.orig` (never
`git checkout`) and confirmed byte-identical with `shasum -a 256`.

| # | Mutation | Result | Test that caught it |
|---|---|---|---|
| TK0217-M1 | `job_spec` puts the bare "A `read` with no range" back | ✅ 1 failed (rc=100) | `a_shell_job_set_names_the_read_op_not_the_read_tool` (`` `job` must say its `read` is an op, not the file tool ``) |

