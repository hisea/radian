# `/jobs` 只列正在运行的后台 job

- 状态：decided＼�`/jobs` 显示的内容太多，只显示现在在运行的后台 job）
- 涉及 ADR：0037（"没有 `list`"一段里关于 `/jobs` 的那句）
- 来源：

## 背景（已核实）

- `pi/shell-job` 合并后，前台命令留存的输出与后台 job 进了同一个登记表（ADR 0037 决定 3a），`Jobs::list`（`crates/radian-coding/src/jobs.rs` ~603）列出全部条目：运行中、已结束的后台 job，以及已结束的前台条目。TUI 的 `/jobs`（`crates/radian-tui/src/command.rs` 的 `jobs_request`，列表由 loop 取 `list()`）原样显示，条目太多。
- 前台条目进登记表时就是已结束状态（`foreground.rs` 调 `insert_settled`），所以"只列运行中"同时去掉了前台条目和已结束的后台 job。
- ADR 0037 现在写着"`/jobs` 列出后台 job，也列出已结束的前台命令留存的输出……"＼�本项推翻它）。

## 决定（已定）

1. `/jobs` 只列状态为运行中的后台 job；没有运行中的就说一行"no running background jobs"。
2. `/jobs kill <id>` 与 `/jobs kill all` 行为不变（kill 已结束的 job 照旧是无操作并说明）。
3. 模型侧不变：`job` 工具照旧能按 id 读已结束的后台 job 和前台留存输出；`Jobs::list` 若还有别的调用方（宿主完成通知、测试），保留它，给 TUI 用一个只取运行中条目的视图或在 TUI 侧过滤。
4. 同分支 docs-only commit：ADR 0037 那句改为"`/jobs` 只列正在运行的后台 job"，并删掉本文件与索引行。

## 验收

- pty 或 TUI 单测：一个运行中的后台 job、一个已结束的后台 job、一个前台留存条目同时存在时，`/jobs` 只显示运行中的那一个；全部结束时显示"no running background jobs"。
- 变异：去掉过滤 → 测试失败。
