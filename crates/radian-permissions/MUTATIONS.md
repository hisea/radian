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
