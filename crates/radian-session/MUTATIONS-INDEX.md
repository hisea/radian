# 变异记录：session index（Phase 1 / 切片 B）

日期：2026-10-12。全部变异在一次性检出 `/tmp/index-mut`（`git worktree add`，HEAD = 本提交）里做：变异 → `cargo test -p radian-session --no-fail-fast`（筛到对应测试）→ `git checkout --` 恢复。提交前两处 `git status --short` 均为空（主 worktree 与一次性检出）。测试一律用 `--no-fail-fast`。

| # | 变异 | 应体现的要求 | 抓住它的测试 |
|---|---|---|---|
| M1 | `index_file` 写入 `byte_offset = event.byte_offset + 1`（索引里存一个文件里没有的事实） | 每个索引字段可由 JSONL 派生 | `every_indexed_field_is_derivable_from_the_jsonl` FAILED |
| M2 | 重建时 `file_bytes` 用整文件大小（含 torn tail），漏掉 §3.2 规则 | torn tail 不进索引 | `a_torn_tail_is_not_indexed_and_is_picked_up_after_truncate_and_append` FAILED |
| M3 | `reconcile` 把 JSONL 覆盖成索引内容 | 不一致时以 JSONL 为准、绝不改 JSONL | `a_stale_index_is_detected_and_rebuilt_from_the_jsonl` FAILED（JSONL 字节不再不变） |
| M4 | `ExportOptions::new()` 默认 `include_provider_payload: true` | 导出默认剔除 raw provider payload | `json_export_omits_provider_payload_by_default_and_includes_it_when_opted_in` FAILED |
| M5 | 只有在**不含** payload 时才脱敏（把开关当脱敏开关） | 脱敏不可关闭 | `secrets_never_appear_in_any_export` FAILED |
| M6 | `list` 忽略 workspace 过滤（`OR 1 = 1`） | workspace 隔离 | `sessions_are_isolated_by_workspace` FAILED |
| M7 | `PRAGMA journal_mode=DELETE`（去掉 WAL） | WAL + busy timeout | `the_index_uses_wal_and_a_busy_timeout` FAILED |
| M8 | `visible_text` 把工具结果文本也当作可检索文本 | 检索范围：工具结果**不**覆盖 | `search_covers_user_and_assistant_text_but_not_tool_results` FAILED |
| M9 | `escape_like` 直接返回查询（不转义 `%`/`_`） | 检索字面匹配 | `like_metacharacters_in_a_query_are_matched_literally` FAILED |

结论：9 个变异全部被抓住，没有等价变异或测试漏洞。

**一次自我修正**：M3 最初把 `reconcile` 写坏时，`a_stale_index_is_detected...` 直接在仓库 fixture `docs/schema/fixtures/v1-minimal.jsonl` 上跑，坏实现把 fixture 改了——这暴露了测试卫生问题（测试不该在仓库文件上做可能被写的操作）。已把该测试改为先 `std::fs::copy` 到临时目录再操作，并重跑了全部 9 个变异（上表为修正后的结果）；一次性检出与仓库的 fixture 均已恢复（`git status` 为空）。
