# 工具小修：grep 输出模式、patch 歧义位置、shell 默认上限

- 状态：decided
- 涉及 ADR：无（ADR 0037 未写明 64 KiB 默认值，改上限不需要动 ADR；若落地分支决定把默认值写进 ADR，则同分支修订）
- 来源：`docs/research/2026-10-18-area-deep-dive/2-tools.md` P6、P5 上半

## 背景

- `grep` 没有总字节上限，也没有 `files`/`count` 模式（`crates/radian-coding/src/provider.rs:73-75`、`grep.rs:22`）。本机 158 个会话的 1,388 次 `grep` 调用（2026-10-20 统计）：单次结果最大 204 KB，86 次超过 20 KB，155 次撞到 200 条匹配上限；53% 的调用带 `context`，其中 206 次 `context ≥ 10`，大结果主要来自上下文行。574 次主动给了 `max_matches`，540 次小于默认 200；shell 里另跑了 439 次 `rg`/`grep`，flag 里 `-c` 57 次、`-l` 17 次。
- `patch_ambiguous` 只说"出现 N 次"，不给位置（`patch.rs:104-115`）。
- 前台 shell 默认每流 64 KiB（`provider.rs:94`）；现在 `job read`（`tool-definitions-trim.md` 落地前叫 `shell_output`）能取回全文（ADR 0037 §3a），大上限的理由已弱一半。

## 决定（已定）

1. `grep` 加总输出字节上限（建议 24–32 KiB，值由落地时定）。超上限时**先丢上下文行、保留匹配行，再砍匹配数**，结果写明丢了什么。加可选的 `output: content | files | count` 模式，**默认保持 `content`**；`files` 返回路径与每个文件的匹配数，`count` 只返回总数与文件数。描述里把「在哪些文件里」「有多少处」直接指向这两个模式＼�依据上面的调用数据）。
2. `patch_ambiguous` 列出前 5 处的行号与每处首行内容。
3. 前台 shell 默认输出上限下调（今天每流 64 KiB；目标 16–32 KiB，值由落地时定），失败时多给尾部。

## 待决问题（未定）

- **`patch` 第 4 层容忍缩进**（报告 P5 下半）：今天到 `LineTrimmed`（只忽略行尾空白）为止，Codex 的 `seek_sequence` 第三档两端 trim。两种做法：只定位、照写 `new`（符合描述里「只定位不发明」的承诺，建议）；或按匹配到的缩进差平移 `new`。定了再并入决定 4。

## 验收

- 单行超长/匹配超多的仓库里 `grep` 输出有界；带 `context: 10` 且 200 条匹配时输出仍不超上限，且匹配行一条不少；`files` 只返回路径与匹配数；`count` 只返回数字。变异：先砍匹配再砍上下文 → 测试失败。
- 歧义 patch 报错里有 5 处行号与首行。
- 1 MB 输出的命令：返回有界、可经 `job read`（落地前 `shell_output`）分页读完、内容与原输出一致。
