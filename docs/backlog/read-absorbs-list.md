# `read` 读目录即列目录，`list` 退为别名

- 状态：idea
- 涉及 ADR：0043（§4 工具数与 `explore` 的只读工具集）、0039（fd 级目录打开不变）
- 来源：Fable 2026-10-20 对 `docs/research/2026-10-18-area-deep-dive/2-tools.md` 的追问＼�核对结果是 read/list 重复）

## 背景

- `read` 今天遇到目录返回 `not_a_file`（`crates/radian-coding/src/read.rs`，`EntryKind::File` 检查）；列目录只能用 `list`。
- 实测（`docs/research/2026-10-15-tool-concurrency-measurement.md` 四个会话）：`read` 206 次，`list` 12 次，`grep` 0 次。`list` 占一个工具位和约 635 B 定义，几乎不用。
- omp 和 opencode 的 `read` 读目录就返回清单；omp 的提示词写明"用 `read` 列目录，不要 `ls`"。pi 的 `ls` 是可选工具。

## 待决方向（建议，未定）

- `read` 收到目录时返回今天 `list` 的输出（排序、kind、size、symlink 不跟随、`max_entries` 上限）；`list` 按 0043 决定 3 的别名机制一个版本内接受并提示新名。
- `explore` 子会话的只读工具集从 `read list glob grep` 变成 `read glob grep`。
- 和 `tool-definitions-trim.md`（shell/job 合并）一样碰 0043 §4 的工具数，等那一项合并后再做，避免三方冲突。

## 验收

`read` 一个目录得到和旧 `list` 一样的结果且走 fd 级打开；`read` 文件行为不变；`list` 旧名得到改名提示且不出现在请求 schema 里；0043 钉住的集合数减一；变异：目录分支去掉后测试失败。
