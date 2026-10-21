# 变异记录：ADR 0019 替换提交/中止与唯一状态机

日期：2026-10-13。一次性检出 `/tmp/0019-mut`（base `8858046`）。每个变异用 `git checkout --`
恢复、`git status --short` 为空；测试一律 `--no-fail-fast`。

| # | 变异 | 结果 | 抓住它的测试 |
|---|---|---|---|
| M1 | `replace_transcript` 不写 `TranscriptReplacementCommitted`（只交换内存） | ✅ failed | `replacing_the_transcript_journals_from_zero_emits_one_event_and_continues`、`a_failed_commit_append_keeps_memory_and_requires_recovery` |
| M2 | 文件结束时的待定段被当成"已写入前缀"接受（`finish` 把 pending 装进 history） | ✅ failed | `a_pending_segment_at_end_of_file_recovers_the_old_history` |
| M3 | 待定段内允许第二个 `TranscriptReplaced` | ✅ failed | `a_second_marker_inside_a_pending_segment_is_a_replay_error` |

3/3 抓住。M1–M3 是 brief 列出的 Core 侧三项。M3 只改了 Core 的状态机；reader 的对应用例由
`radian-session/MUTATIONS.md` 的 M-R1 覆盖（reader 绕过状态机时同一批用例失败）。
