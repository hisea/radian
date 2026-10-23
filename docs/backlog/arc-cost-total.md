# arc：task 与 plan 的总开销，review 计入预算

- 状态：decided（owner 定：review 的花费计入预算；task 与 plan 都要有总开销。字段名与显示方式是 lead 的设计）
- 涉及 ADR：0048（成本与预算）、0044

## 背景

task 的 `cost_usd` 今天只是 worker attempt 的花费之和。review stage 的花费另记在每个 verdict 的 `review_cost_micros` 里，`show` 只给最新一次，前几轮 review 的花费没人加总，也不计入预算：review 跑多少轮都不会让 task 暂停。`reset-budget` 把 `cost_usd` 清零，所以它是预算计数，不是 task 一共花了多少。plan 的预算在检查时把各 task 的 `cost_usd` 加起来，但这个和不输出，`list`、`show` 都看不到 plan 花了多少。

## 决定（已定）

1. **review 计入预算。** 每一次 review stage 的花费（它跑的所有 session 之和）在 verdict 写下时加进 task 的 `cost_usd`，并当场按 task 与 plan 的 `max_cost_usd` 检查：到了上限，task 在这次 gate 结束后停在 `todo.paused_budget`，不再开返工；plan 的上限照旧把 plan 的 `auto` 设为 `off`。gate 重跑、reverify 里跑的 review 也一样计入。
2. **一个不清零的总额。** task 新增 `total_cost_usd`：worker attempt 与每一次 review 的花费之和，`reset-budget` 不改它。`cost_usd` 照旧是预算计数（现在含 review），`reset-budget` 照旧清零它。
3. **plan 的总额。** plan 在 `list` 与 `show` 里给出 `cost_usd`（各 task `cost_usd` 之和，即预算比较的那个数）与 `total_cost_usd`（各 task `total_cost_usd` 之和）。它们是读时算出的，不写进 plan 文件。
4. **显示。** `show` 的文本与 `--json`、`list --json` 都带 task 与 plan 的两个数；`list` 的文本形式在每个 plan 行给出 `total_cost_usd`。
5. 一次 review 的运行没有报出花费（`review_cost_micros` 为 `null`）时，它加 0，与 worker attempt 的规则相同。
6. 已有 task 没有 `total_cost_usd` 时，第一次写入前按 `cost_usd` 起算；不回头去读旧 verdict 补算。

## 验收

- 一个 task 两次 attempt、三次 review：`total_cost_usd` 等于五者之和；`reset-budget` 之后 `cost_usd` 为 0，`total_cost_usd` 不变。
- `max_cost_usd` 只靠 review 的花费到达时，task 在 gate 结束后停在 `todo.paused_budget`，不开 review 返工。
- plan 的两个数等于其 task 之和，出现在 `list`、`list --json`、`show`。
- 手册：`arc-files.md` 的 `cost_usd` 一行改写、新增 `total_cost_usd`，`limits.max_cost_usd` 一行说明 review 计入；`arc.md` 的 `list`/`show` 键列表与 `reset-budget` 段落改写。ADR 0048 的成本段落在同一分支的 docs-only commit 里改写。
