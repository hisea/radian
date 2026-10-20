# 文件工具的同步 IO 放进 `spawn_blocking`（先做基准）

- 状态：idea
- 涉及 ADR：无（`radian-coding` 内部实现）
- 来源：`docs/research/2026-10-18-area-deep-dive/2-tools.md` P10 与 §5.10

## 背景

`read`/`grep`/`glob`/`list` 的 `async fn` 里是同步 IO（`radian-coding` 里没有 `spawn_blocking`，只有 `radian-host/src/contrib.rs` 用了一处），Core 在同一个任务里用 `FuturesUnordered` 轮询，所以同一批里的几个 Shared 文件工具**可能是串行执行的**。这是读源码的推断，没有实测。工具描述说它们「run in parallel」。

调用数据（本机 158 个会话，2026-10-20）：`grep` 共 1,388 次，是第三大工具；40% 的 grep 后面紧接着另一个 grep，其中 535 次是换 pattern 的新搜索；同一条回复里放 2–3 个 grep 的只有 16%。所以并行要两层都到位：模型把独立搜索放进同一条回复（靠 `shell-auto-background-60s.md` 里那句提示词），运行时真正并发执行（本项）。

## 待决方向

顺序：先落提示词那一句 → 写基准（三个并发 grep 在本仓库和一个大仓库上的墙钟时间，和单次比）→ 串行就把扫描放进 `tokio::task::spawn_blocking`，不串行就把描述里的「run in parallel」改准确。`spawn_blocking` 不改 `agent-core`，只改 `radian-coding` 内部。

## 验收

基准数字进 `docs/perf-baseline.md`；三个并发 grep ≈ 单次而不是三倍。
