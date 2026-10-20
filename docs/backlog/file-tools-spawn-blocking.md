# 文件工具的同步 IO 放进 `spawn_blocking`（先做基准）

- 状态：decided（已落地，分支 `dogfood/file-tools-parallel`，待合入；`read` 不在此项，仍在 `read-absorbs-list.md` 那边）
- 涉及 ADR：无（`radian-coding` 内部实现）
- 来源：`docs/research/2026-10-18-area-deep-dive/2-tools.md` P10 与 §5.10

## 背景

`read`/`grep`/`glob`/`list` 的 `async fn` 里是同步 IO（`radian-coding` 里没有 `spawn_blocking`，只有 `radian-host/src/contrib.rs` 用了一处），Core 在同一个任务里用 `FuturesUnordered` 轮询，所以同一批里的几个 Shared 文件工具**可能是串行执行的**。这是读源码的推断，没有实测。工具描述说它们「run in parallel」。

调用数据（本机 158 个会话，2026-10-20）：`grep` 共 1,388 次，是第三大工具；40% 的 grep 后面紧接着另一个 grep，其中 535 次是换 pattern 的新搜索；同一条回复里放 2–3 个 grep 的只有 16%。所以并行要两层都到位：模型把独立搜索放进同一条回复（靠 `shell-auto-background-60s.md` 里那句提示词），运行时真正并发执行（本项）。

## 决定（已定）

顺序：先落提示词那一句 → 写基准（三个并发 grep 在本仓库和一个大仓库上的墙钟时间，和单次比）→ 串行就把扫描放进 `tokio::task::spawn_blocking`，不串行就把描述里的「run in parallel」改准确。`spawn_blocking` 不改 `agent-core`，只改 `radian-coding` 内部。

## 验收

- 基准数字进 `docs/perf-baseline.md`：单次 grep、同一批三个 grep 的墙钟时间，改前改后各一组，本仓库和一个大仓库各一次。
- 改后三个并发 grep 的墙钟 ≈ 单次而不是三倍；`read`/`glob`/`list` 同样处理。
- 取消仍然生效：批次中途取消时，`spawn_blocking` 里的扫描不会把结果写回（Core 的取消语义不变）。
- 变异：去掉 `spawn_blocking` 后基准测试（或一个用慢文件系统模拟的测试）失败。
- 若基准证明本来就不串行，则只改描述文字，本项以 docs-only 收尾。

## 落地（分支 `dogfood/file-tools-parallel`，2026-10-20；本文件按任务要求保留不删）

- 提示词一句已落：`crates/radian-host/prompts/default.md` 加了“Send independent searches
  and reads in the same reply so they run together.”，`prompt.rs` 的
  `the_builtin_default_role_carries_its_rules` 用 "in the same reply so they run together"
  钉住。
- 基准（`crates/radian-coding/tests/perf_file_tools.rs`，全部 `#[ignore]`，debug）：同一任务
  `tokio::join!` 三个 grep（Core 批量的形状），needle 全树命中不了。改前本仓库单次
  162.6 ms / 三个 504.8 ms（3.10×），registry 最大展开树（601 MB）单次 1.89 s / 三个
  5.83 s（3.08×）：**串实锤**。改后 174.8 ms / 200.9 ms（1.15×）与 1.77 s / 1.57 s
  （0.89×）：**并行**。数字在 `docs/perf-baseline.md`“文件工具 `spawn_blocking`”两节。
- `grep`/`glob` 的 `run` 只做参数解析与限额计算，fd walk + 逐行读 + 渲染搬进
  `tokio::task::spawn_blocking`（`agent-core` 不动；取消时丢 future，阻塞侧只读跑完即弃，
  结果不写回）。`shell` 描述里的 "run in parallel" 经基准证实，无需改字。
- 重叠证明不依赖墙钟（`crates/radian-coding/tests/file_tools_parallel.rs`）：
  `two_greps_in_one_batch_overlap` / `two_globs_in_one_batch_overlap` 用既有
  `safe_fs::resolve_hook` 里的条件变量会合；去掉 `spawn_blocking` 后两者各以 10 s
  hook 超时失败（已做变异，见下）。原 seam 在静态锁里调 hook，两个扫描永远会合不了，
  改为 clone 出 `Arc` 再调（只改 `testing` 特性下的调用方式，`resolve_hook` 签名不变，
  `safe_fs.rs` 的 swap 测试全过）。
- 取消（`a_cancelled_batch_returns_without_the_scan_result`）：hook 按住扫描时取消赢得
  竞争，放行后扫描仍在后台跑完并找到匹配，但结果从不是返回值。
- 变异：`grep.rs` 去掉 `spawn_blocking` → `two_greps_in_one_batch_overlap` 失败；
  `glob.rs` 去掉 → `two_globs_in_one_batch_overlap` 失败；恢复后 `git diff` 确认还原。
- `read` 未动：`pi/read-absorbs-list` 正在把 `list` 并进 `read`，等那条分支落地后 `read`
  再做同样的 `spawn_blocking` 处理。
