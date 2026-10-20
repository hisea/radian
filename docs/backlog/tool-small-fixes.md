# 工具小修（剩余）：grep 先丢上下文、前台 shell 默认上限、patch 缩进容忍（待决）

- 状态：decided
- 涉及 ADR：无（ADR 0037 未写明 64 KiB 默认值，改上限不需要动 ADR；若落地分支决定把默认值写进 ADR，则同分支修订。缩进层无 ADR 约束，落地时同步改工具描述）
- 来源：`docs/research/2026-10-18-area-deep-dive/2-tools.md` P6、P5；依据 grep 调用数据的决定

## 背景

- 已落地（`pi/tool-small-fixes`）：`grep` 总字节上限（`Limits::max_grep_output_bytes`，64 KiB，按顺序截断整行、保证不超上限）与 `output: content | files | count` 模式（默认 `content`）；`patch_ambiguous` 列出前 5 处行号与首行。
- 本机 158 个会话的 1,388 次 `grep` 调用（2026-10-20 统计）：单次结果最大 204 KB，86 次超过 20 KB，155 次撞到 200 条匹配上限；53% 的调用带 `context`，其中 206 次 `context ≥ 10`，大结果主要来自上下文行。574 次主动给了 `max_matches`，540 次小于默认 200；shell 里另跑了 439 次 `rg`/`grep`，flag 里 `-c` 57 次、`-l` 17 次。
- 已落地的截断是按输出顺序整行停下，上下文行与匹配行一视同仁：带大 `context` 时，靠后的匹配行会先于上下文被砍掉。
- 前台 shell 默认每流 64 KiB（`provider.rs:94`）；现在 `job read`（`tool-definitions-trim.md` 落地前叫 `shell_output`）能取回全文（ADR 0037 §3a），大上限的理由已弱一半。
- `patch` 第 3 层只忽略行尾空白，不容忍缩进偏差；Codex 的同档能容忍。

## 决定（已定）

1. `grep` 超上限时**先丢上下文行、保留匹配行，再砍匹配数**，结果写明丢了什么（丢了多少上下文行、多少匹配）。上限从 64 KiB 降到 24–32 KiB（值由落地时定）。工具描述里把「在哪些文件里」「有多少处」直接指向 `output: files` / `count`＼�依据上面的调用数据）。
2. 前台 shell 默认输出上限下调（今天每流 64 KiB；目标 16–32 KiB，值由落地时定），失败时多给尾部。等 `pi/shell-job` 合并后在新的 main 上做。

## 待决问题（未定）

- **`patch` 第 4 层容忍缩进**（报告 P5 下半）：今天到 `LineTrimmed`（只忽略行尾空白）为止，Codex 的 `seek_sequence` 第三档两端 trim。两种做法：只定位、照写 `new`（符合描述里「只定位不发明」的承诺，建议）；或按匹配到的缩进差平移 `new`。定了再并入决定 3。

## 验收

- 带 `context: 10` 且 200 条匹配时输出不超上限，且匹配行一条不少（上下文先被丢、结果写明丢了多少）；变异：先砍匹配再砍上下文 → 测试失败。
- 工具描述里有指向 `files`/`count` 的一句。
- 1 MB 输出的命令：返回有界、可经 `job read`（落地前 `shell_output`）分页读完、内容与原输出一致。
