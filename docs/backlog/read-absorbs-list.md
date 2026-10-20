# `read` 读目录即列目录，`list` 退为别名

- 状态：decided＼�
- 涉及 ADR：0043（§4 工具数、`explore` 的只读工具集、别名清单）、0039（fd 级目录打开不变）
- 来源：Fable 2026-10-20 对 `docs/research/2026-10-18-area-deep-dive/2-tools.md` 的追问＼�核对结果是 read/list 重复）；决定做

## 背景（已核实）

- `read` 今天遇到目录返回 `not_a_file`（`crates/radian-coding/src/read.rs`，打开后检查 `EntryKind::File`）；列目录只能用 `list`（`list.rs`：`path?`、`max_entries?` 默认 500，按名排序，kind 与 size，symlink 显示不跟随）。
- 实测（`docs/research/2026-10-15-tool-concurrency-measurement.md` 四个会话）：`read` 206 次，`list` 12 次，`grep` 0 次。`list` 占一个工具位和约 635 B 定义。
- omp 和 opencode 的 `read` 读目录就返回清单；omp 的提示词写明"用 `read` 列目录，不要 `ls`"。pi 的 `ls` 是可选工具。
- 别名机制（ADR 0043 决定 3，`git_status` 的做法）：旧名留在 provider 的 `tool_specs()` 里让 Core 的 preflight 接受调用，`radian-host/src/model.rs` 的 `strip_renamed_tool_aliases` 从每个请求里剥掉，调用得到一条「已改名：用 X」的回复（`git.rs` 第 98 行的形状），不执行旧功能。
- `explore` 子会话钉死为 `read list glob grep` 四个（`radian-host/src/subagent.rs` 与 `tests/subagent_mcp.rs`）。

## 决定（已定）

1. `read` 的 `path` 指向目录时，返回今天 `list` 的输出（同样的排序、kind、size、symlink 不跟随、同一条 fd 级打开路径）。**schema 不加字段**：`limit` 对文件是行数、对目录是条目数上限（缺省 `default_list_entries` = 500）；`offset`/`max_bytes` 对目录忽略并在结果里说明。只读根（cargo registry / rustup 源码）下的目录同样可读。
2. `list` 按 0043 决定 3 退为**一个版本的别名**：留在 `tool_specs()`，从请求里剥掉，调用回复「`list` 已改名：用 `read` 读目录」。`<runtime>` 层的工具清单（`radian-host/src/tools.rs`）去掉 `list` 行，`read` 一行写明可读目录。
3. `explore` 子会话的只读工具集改为 `read glob grep`（三个）；0043 §1a 与 §4 的文字同步。
4. 0043 §4 的工具数减一；与 `pi/shell-job` 都改这一行，**基于 shell-job 合并后的 main 开分支**，由 gate 调和数字。
5. `read` 的描述加一句"a directory path lists it"，不复述 `list` 的全部文字；定义字节数不超过今天 `read` + 150 B。

## 验收

- `read` 一个工作区目录与一个只读根目录，输出与旧 `list` 逐字一致；`limit` 生效；symlink 不跟随；超过 500 条截断并说明。
- `read` 文件的行为与所有现有测试不变。
- `list` 旧名：请求 schema 里不出现，调用得到改名回复；`strip_renamed_tool_aliases` 的测试覆盖它。
- 0043 钉住的两个集合各减一；`explore` 子会话工具表恰为 `read glob grep`，`list` 在子会话里也被剥掉。
- 变异：目录分支去掉 → 测试失败；`limit` 不作用于目录 → 失败；`list` 未从请求剥掉 → 失败；explore 仍挂 `list` → 失败。
- 全套 Verify（AGENTS.md）。
