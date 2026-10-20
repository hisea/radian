# 工具小修：grep 输出模式、patch 歧义位置、shell 默认上限

- 状态：decided
- 涉及 ADR：无（ADR 0037 未写明 64 KiB 默认值，改上限不需要动 ADR；若落地分支决定把默认值写进 ADR，则同分支修订）
- 来源：`docs/research/2026-10-18-area-deep-dive/2-tools.md` P6、P5 上半

## 背景

- `grep` 没有总字节上限，也没有 `files`/`count` 模式（`crates/radian-coding/src/provider.rs:73-75`、`grep.rs:22`）。
- `patch_ambiguous` 只说"出现 N 次"，不给位置（`patch.rs:104-115`）。
- 前台 shell 默认每流 64 KiB（`provider.rs:94`）；现在 `job read` 能取回全文（ADR 0037 §3a），大上限的理由已弱一半。

## 决定（已定）

1. `grep` 加总输出字节上限（超了截断并提示）；加可选的 `files`/`count` 模式，**默认保持 `content`**。
2. `patch_ambiguous` 列出前 5 处的行号与每处首行内容。
3. 前台 shell 默认输出上限下调（今天每流 64 KiB；目标 16–32 KiB，值由落地时定），失败时多给尾部。

## 验收

- 单行超长/匹配超多的仓库里 `grep` 输出有界；`files` 只返回路径与匹配数。
- 歧义 patch 报错里有 5 处行号与首行。
- 1 MB 输出的命令：返回有界、可经 `shell_output` 分页读完、内容与原输出一致。
