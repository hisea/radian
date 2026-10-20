# `exec --output jsonl` 的 `Result` 增加本次运行的用量：`run_usage`、`run_cost`、`run_turns`

- 状态：decided＼�外部审核 P2-2 采纳）
- 涉及 ADR：0044（`Result` 记录）、0007（usage 形状）、0006
- 来源：外部审核 2026-10-20 第三节 P2-2

## 背景

ADR 0044 把 `turns`、`usage`、`cost` 定义为会话累计（含 `--resume` 之前）。benchmark、CI 与外部编排要的是**本次进程**的消耗；用累计值做指标，续接会话会重复计费。

## 决定（已定）

- `Result` 新增三个字段：`run_turns`、`run_usage`、`run_cost`，形状与现有三个相同，只统计**本进程写入**的 journal 行。边界就是 tee 安装时返回的 `written_len`：它之前的字节是上次留下的，之后的是本次运行的。不新增 journal 记录。
- 现有三个字段不改名、不改义，保持会话累计；文档里两组并列说明。
- 没有 resume 时两组相等；fixture 里各放一条有 resume 与没有 resume 的样例。

## 验收

- `cost_resume` 类测试：resume 后跑一轮，`usage` 含两轮、`run_usage` 只含一轮；无 resume 时两组字节相同。
- `docs/schema/fixtures/exec-result.jsonl` 更新并通过 `check-fixtures.sh`；ADR 0044 §2 表格补三行（docs-only commit）。
