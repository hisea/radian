# arc：返工开新 session，不接着旧上下文跑

- 状态：decided（owner：先做；缺省 `fresh`，可配置回 `resume`）
- 涉及 ADR：0048（§7 worker、返工）、0018（自动压缩）
- 关联：`arc-reviewer-profiles.md`（返工轮只审增量）、`compaction-threshold.md`

## 背景

worker 的 session id 就是 task id，返工用 `--resume` 接着同一个 session 跑。一次夜间运行的 journal：

| task | 第 1 轮开始时的 prompt | 最后一轮开始时的 prompt | 最后一轮 |
|---|---|---|---|
| TK0017 | 1.1 万 tokens | 57 万 | 改一行 help 文字：12 次调用，共 690 万输入 tokens |
| TK0021 | 1.0 万 | 68 万 | |
| TK0013 | 1.0 万 | 53 万 | |

模型窗口有 98 万 tokens，所以一次自动压缩都没有触发。缓存命中让成本不高，但每次调用都要处理几十万 tokens：每轮更慢，模型要在很长的上下文里找到最后那几行返工说明。

## 想法

1. 每次返工开一个新 session（id 形如 `<task>-<attempt>`，与 review 会话的命名一致），stdin 给：
   - 任务原文 brief；
   - 最新的返工说明；
   - 上一轮 worker 的最终报告；
   - 当前分支相对 merge-base 的 diff 摘要（`--stat` 加提交列表）。

   估计开工时 2–4 万 tokens。
2. 可选：留一个配置，`[worker] rework_session = "fresh" | "resume"`，缺省 `fresh`。
3. TUI 的 conversation 标签和 attach 按 attempt 列出 session（与 TK0018 计划中的 review 会话选择器同一处）。

## 决定（已定）

1. 每次返工开新 session，第 1 条的输入照想法；缺省就是这样。
2. 配置 `[worker] rework_session`，闭集 `fresh`、`resume`，缺省 `fresh`；`resume` 保留今天的行为。
3. 压缩方案（resume 前超阈值先压缩）不在本项，留给 `compaction-threshold.md`。
