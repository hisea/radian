# 工具定义瘦身：一个 `job` 工具替掉四个

- 状态：in-progress（pi/shell-job）
- 涉及 ADR：0037、0043、0039、0006
- 来源：`docs/research/2026-10-18-area-deep-dive/2-tools.md` P4；决定（对手核对：opencode 1 个工具、omp 2 个、Codex 2 个、pi 1 个且无后台；Radian 5 个）

## 背景

围绕 shell 挂了 5 个工具：`shell`、`job_output`、`job_list`、`job_kill`、`shell_output`，合计约 8 KB。`shell_output` 和 `job_output` 共用同一组按行读参数，只是一个收 handle、一个收 job id；根因是前台留存输出和后台 job 是两套 id 空间。`job_list` 对模型没用（id 已在上下文里，对手都不给）。`shell` 的描述把后台规则讲了三遍。`wait: true` 与完成通知是两条等待路径，描述却只教前者。

## 决定（已定，写在 ADR 0037 决定 3a/4）

- `shell` 只负责起命令；结果里只有一种 id（job id），不分前台/后台。后台与转后台的结果带「完成时会通知你；不要轮询；没别的事就结束回答」。
- `job { id, op: read | wait | kill }` 是唯一的后台工具。`read` 不带范围参数是增量 ring + `[status]`，带 `tail_lines` / `from_line`+`max_lines` / `pattern` 之一按行读全文。`wait` 保留但描述里是次选。没有 `list`（TUI `/jobs` 留给人看）。
- 前台截断输出登记为同一注册表里已 settle 的条目：不占名额、不写 journal、留存上限与淘汰规则不变。
- 四个旧名按 0043 决定 3 的别名机制一个版本内接受并提示新名。
- `shell` ≤ 2,200 B、`job` ≤ 1,800 B（紧凑 JSON 口径），由测试钉上限。

## 未定（本项之外）

其余工具描述的瘦身（`ask_user` 的宿主校验约束、`<runtime>` 层的工具清单重复）等本项落地后重新量字节数再定。memory 三个工具的合并随 `pi/memory-tools` 消失，不再是本项内容。

## 验收

ADR 0037 的验证段；0043 钉住的两个集合改成 15 个；评测集上 shell 相关任务成功率不降、`job_output` 旧名调用得到改名提示。
