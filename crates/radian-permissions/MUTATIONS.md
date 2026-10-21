# 变异记录：radian-permissions（Phase 3）

日期：2026-10-12。变异在一次性检出 `/tmp/perms-mut`（`git worktree add`，HEAD = 本提交）里做：
改完跑对应测试、`git checkout` 恢复，提交前 `git status --short` 为空。测试用 `--no-fail-fast`。

| # | 变异 | 应体现 | 抓住它的测试 |
|---|---|---|---|
| PM1 | `evaluate` 的优先比较反过来（`<` 而非 `>`），allow 覆盖 deny | deny 永远赢 | `rule.rs::priority_deny_beats_ask_beats_allow_and_default_is_ask` FAILED |
| PM2 | `simple_prefix_matches` 去掉 simple 检查、改用与 deny 相同的分隔符归一化 | 复合命令不被 allow 放行 | `rule.rs::shell_metacharacters_are_never_allowed` FAILED |
| PM3 | 命令 `deny` 改用 `simple_prefix_matches`（不再逐段保守匹配） | 复合命令任一段命中即 deny | `rule.rs::a_deny_segment_denies_the_whole_composite_command` FAILED |
| PM4 | `resolve` 直接返回 join 后的路径，不解析 symlink/`..` | 规则按解析后路径匹配、`..`/symlink 不能绕过 | `rule.rs::rules_match_resolved_paths_so_dotdot_and_symlinks_cannot_bypass` FAILED |
| PM5 | 会话授予不参与判定（`granted = false`） | 授予后下一个调用立即生效 | `hooks.rs::a_session_grant_applies_to_the_next_call_and_not_after_resume` FAILED |
| PM6 | 授予检查去掉 `human &&`（非人类来源可借用） | 非人类来源借不到授予 | `hooks.rs::a_non_human_source_cannot_ask_or_borrow_a_grant` FAILED |
| PM7 | approver 出错时返回 `Ok(Allow)` | hook 内部错误 → 拒绝 | `hooks.rs::an_internal_error_denies` FAILED |
| PM8 | 去掉 `tokio::select!` 的取消分支，直接 await approver | 取消要立即返回、不挂住 | `hooks.rs::cancellation_interrupts_the_approver` FAILED（测试 2s 超时后 `expect` 失败） |
| PM9 | `grants` 改为进程级 `OnceLock` 共享，跨实例可见 | 授予不跨 resume | `hooks.rs::a_session_grant_applies_to_the_next_call_and_not_after_resume` FAILED |

结论：9 个变异全部被抓住，没有等价变异或测试漏洞。PM8 的失败是测试里的看门狗超时（2s）而不是挂死；
PM9 在第一次跑时输出为空（变异脚本本身写坏导致编译失败），改成一个合法变异后复现了失败。

## ADR 0021 M1 第三片：记忆工具的权限（2026-10-13）

Base `2d01d9b` · 一次性 worktree `/tmp/memsvcb-mut` · 独立 `CARGO_TARGET_DIR` ·
`cargo test -p radian-permissions`，变异后 `git checkout --` 还原。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M9 | `memory_revise` 规则 `Ask` → `Allow` | ✅ 抓住 | `profile::tests::the_memory_tools_read_allowed_and_write_asked_in_every_tier` |

## ADR 0024 Q6：`ask_user` 固定允许（2026-10-13）

Base `4866944` · 一次性检出 `/tmp/ask-mut`（detached，跑完删除）· `cargo test -p radian-permissions`。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| A4 | `always_allowed()` 返回空（三档都不再固定允许 `ask_user`） | ✅ 抓住 | `profile::tests::ask_user_is_allowed_in_every_tier` |

1/1 抓住。变异后 `ask_user` 落到默认的 `ask`，三档都断言失败。

## ADR 0026 D9：`Auto` 策略与会话内开关（2026-10-13）

Base `43ddbff` · 一次性检出 `/tmp/auto-mut`（detached，跑完删除）· 独立 `CARGO_TARGET_DIR` ·
`cargo test -p radian-permissions --test hooks <test>`，变异后 `git checkout --` 还原。

| # | 变异 | 应体现 | 抓住它的测试 |
|---|---|---|---|
| PM-A1 | `apply_policy` 的 auto 臂改成 `(Auto, _) => Allow`（deny 也放行） | deny 规则在 auto 下照样拒绝 | `hooks.rs::auto_still_honors_deny_rules` FAILED |
| PM-A2 | `set_approval` 去掉 `audit.record_policy_change` | 每次开关写一条审计 | `hooks.rs::set_approval_switches_in_place_and_audits_each_switch` FAILED |

2/2 抓住。owner 的更正把"自动批准键不经确认直接开启"这条变异换成"`Y` 开启了 auto"，
那条变异改的是 `radian-tui` 的键位映射，记录在 `crates/radian-tui/MUTATIONS.md`（TM-A1）。

## ADR 0028 R8：`mermaid_check` 三档 allow（2026-10-14）

Base `cf37140` · 一次性 worktree `/tmp/radian-diag-mut`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| DG-M3 | 把 `mermaid_check` 从 `always_allowed()` 去掉（落到默认 ask） | `profile::tests::mermaid_check_is_allowed_in_every_tier` | ✅ |

1/1 抓住。

## ADR 0030 slice 1：`PermissionResolved.policy` 穿过 `runtime_event`

Base: `a4a00bf` · 改前把 `src/hooks.rs` 复制到 `/tmp/orch-s1-mut-backup/`，改后 `cp` 还原
（本任务禁 `git checkout --`）；`git status --short` 结束后为空。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| PM-O1 | `runtime_event` 传 `None` 而不是 `event.policy.clone()` | `hooks::runtime_event_threads_the_optional_policy_name` | ✅ failed |

1/1 抓住。审计层的 `record` 现在恒为 `policy: None`（还没有策略命令生产者，ADR 0030 切片 ②），
所以这条变异只改 `runtime_event` 的映射，测的就是「字段穿过这一层」这一件事。

## ADR 0030 切片 5：task 硬禁止清单（2026-10-16）

Base `63fafba` · 工作树 `.radian-worktrees/orch-s5`。改前把 `src/profile.rs` 复制到
`.radian/scratch/mut-backup/`，改后 `cp` 还原并以 `cmp` 证明一致（本任务禁 `git checkout --`）。
命令：`timeout 240 cargo test -p radian-permissions --test task_hard_deny --locked --no-fail-fast`。

| # | 变异 | 抓住它的测试 | 结果 |
|---|---|---|---|
| PM-O2 | `with_task_hard_deny` 变成 no-op（硬禁止清单不再叠到 task profile 上） | `task_hard_deny::{the_hard_deny_list_denies_exactly_what_the_plan_lists, a_hard_deny_command_never_reaches_the_approver}` | ✅ GRABBED (rc=101) |

1/1 抓住。硬禁止是 `deny` 规则（优先级最高，`ApprovalPolicy` 不会降级），所以清单移除后
`git push` 落到 `ask`，approver 一允许就通过；这条变异同时也说明清单不能被 edge policy 放宽
（清单在时 approver 根本收不到该调用）。

## ADR 0042：`mermaid_check` 的 `allow` 名单删除（2026-10-16）

上面「ADR 0028 R8：`mermaid_check` 三档 allow」的 DG-M3 已随 ADR 0042 的补充失效：工具删除后
`always_allowed()` 只剩 `ask_user` 与 `skill`，被它测的 `mermaid_check_is_allowed_in_every_tier`
也一并删除，没有新的变异可跑。

## ADR 0043 slice ⓪a：`subagent` 的两档 allow（2026-10-17）

`subagent` 自己不写文件、不跑工作区命令，它起的子会话由子自己的档位与审批决定，所以两档都
`allow`；否则无头父会话会落到默认 `ask` 而无终端可问、被直接拒绝。变异把 `always_allowed()`
还原成 `["ask_user", "skill"]`（`cp` 到 `.radian/scratch/mut/`，改后 `cp` 还原并 `cmp` 一致）。
命令：`cargo test -p radian-permissions --locked --lib subagent_is_allowed --no-fail-fast`。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| SP-1 | `always_allowed()` 去掉 `subagent` | ✅ 1 failed（`workspace` 档 `left: Ask`，`right: Allow`） | `profile::tests::subagent_is_allowed_in_every_tier` |

## ADR 0043 slice ③: a renamed tool is judged under its new name (2026-10-17, removed 2026-10-20)

(Removed with `remove-git-diff`: the alias mapping is gone — `rule::canonical_tool_name` is
deleted, rules match literal tool names, and calls to removed names never reach this layer. The
AR-1 mutation below no longer applies; `rules_against_removed_names_match_only_the_literal_name`
pins the new behaviour.)
