# 文件工具的同步 IO 放进 `spawn_blocking`（先做基准）

- 状态：idea
- 涉及 ADR：无（`radian-coding` 内部实现）
- 来源：`docs/research/2026-10-18-area-deep-dive/2-tools.md` P10 与 §5.10

## 背景

`read`/`grep`/`glob`/`list` 的 `async fn` 里是同步 IO（`radian-coding` 里没有 `spawn_blocking`，只有 `radian-host/src/contrib.rs` 用了一处），Core 在同一个任务里用 `FuturesUnordered` 轮询，所以同一批里的几个 Shared 文件工具**可能是串行执行的**。这是读源码的推断，没有实测。工具描述说它们"run in parallel"。按实测每次约 10 ms，收益很小；大仓库上的 grep 会更明显。

## 待决方向

先写基准：三个并发 grep 在大仓库上的墙钟时间，和单次比。串行就把同步部分放进 `tokio::task::spawn_blocking`；不串行就把描述里的"run in parallel"改准确。注意 `grep` 实测调用次数是 0（模型在 shell 里跑 rg），所以这项的优先级低于让模型用上 `grep`（`tool-small-fixes.md`）。

## 验收

基准数字进 `docs/perf-baseline.md`；三个并发 grep ≈ 单次而不是三倍。
