# Performance baseline (Phase 7)

测量日期：2026-10-16。**只测量，不改产品代码。** 本文件记录一组可重复的基线数字，
以及它们在什么条件下取得。

## 机器与构建

| 项 | 值 |
|---|---|
| 机器 | Apple Mac mini，Apple M4，10 核，32 GiB，macOS 27.0.1（Darwin 27.0.0） |
| Rust | rustc 1.99.0（release 与 debug 两种构建都测） |
| 构建 | `cargo test`（debug）与 `cargo test --release`，均为 workspace 默认 profile，无 `lto`/`codegen-units` 调整 |
| 文件系统 | APFS（`/var/folders` 下的临时目录；`fsync` 数字因此是 APFS 的） |

**重要前提**：这台机器在测量期间同时被多个 worktree / 进程使用，
`uptime` 的 1 分钟负载在 **7 到 58** 之间波动。绝对数字受此影响（尤其 p95），
**相对比较与量级比单个绝对值更可信**。每张表都标注了样本数；所有测量都用
`std::time::Instant`，不用 `sleep` 计时（轮询用的 1 ms 间隔会向上量化，已注明）。

## 复现命令

```sh
# 全部都是 #[ignore]，默认 cargo test 不会被拖慢
cargo test --release -p radian-session --test perf_session -- --ignored --nocapture
cargo test --release -p radian-runtime --test perf_runtime -- --ignored --nocapture
cargo test --release -p radian-tui     --test perf_render  -- --ignored --nocapture
cargo test --release -p radian-tui     --test perf_startup -- --ignored --nocapture --test-threads=1
cargo test --release -p agent-cli      --test perf_agent   -- --ignored --nocapture --test-threads=1
# debug：把 --release 去掉
```

p95 取排序后的第 95 百分位（小样本下即接近最大值，故 p95 对负载尖峰很敏感）。

## 1. 启动与 `--resume`

方法：`perf_startup.rs` / `perf_agent.rs`。`radian-tui` 在 `libc::openpty` 的真实 pty 里
启动，读到输入框标题 `message` 为止；`agent` 用管道 stdin 启动，读到 stderr 的 `> ` 提示符
为止。`--resume` 的会话文件由测量代码按 v1 格式直接写盘（固定输入），最后一条消息是一个
可见标记。各 10 次（startup）/ 5 次（resume），先做一次 warmup 以排除冷启动。无网络
（`--base-url` 指向死端口，不跑回合）。**沙箱探针包含在启动内**（默认 `workspace-write`）。

| 场景 | debug 中位 / p95 | release 中位 / p95 | n |
|---|---|---|---|
| `radian-tui` 启动到输入框 | 97.0 / 104.4 ms | 98.4 / 150.3 ms | 10 |
| `radian-tui --resume` 1k 条 | 109.2 / 113.4 ms | 89.7 / 96.2 ms | 5 |
| `radian-tui --resume` 10k 条 | 529.9 / 561.8 ms | 161.0 / 170.8 ms | 5 |
| `agent` 启动到提示符 | 32.3 / 39.2 ms | 54.9 / 80.4 ms | 10 |
| `agent --resume` 1k 条 | 38.5 / 51.7 ms | 41.5 / 76.6 ms | 5 |
| `agent --resume` 10k 条 | 171.2 / 196.5 ms | 54.4 / 59.7 ms | 5 |
| `workspace-write` 沙箱探针 | 23.7 / 48.6 ms | 26.6 / 52.7 ms | 20 |

启动在 debug 与 release 差别不大（~100 ms），说明它由**进程启动 + 终端初始化 + 沙箱探针**
构成，不是计算；`--resume` 10k 的 debug 明显更慢（529 ms vs 161 ms），因为读取/渲染是
debug 慢路径。沙箱探针约 25 ms，占启动的四分之一左右。

> 方法学说明：最初用 `radian-tui/tests/pty.rs` 的 `portable-pty` 测启动，得到 ~2–5 s，
> 并且 "10k resume 比 1k 快" 这种自相矛盾的结果。逐项拆开后是**测试端**的问题：
> `portable-pty` 让子进程的第一个字节迟到 ~2 s（`env_clear` 已排除），是它 fork 大体积
> 测试进程的开销，不是 app 的。换成 `libc::openpty` + `std::process::Command`
> （posix_spawn）后，第一个字节 ~40 ms、输入框 ~100 ms，与一个独立的 Python pty 探针一致。
> 所以上表用后者；`portable-pty` 的数字不代表 app。

## 2. 首 token 与工具往返

两种口径：

- **端到端（pty，release/debug）**：`perf_startup.rs`，本地 mock HTTP server 立即返回。
  从按下回车到回复文字出现在 pty 字节流（1 ms 轮询量化）。
- **进程内（无 HTTP/终端）**：`perf_runtime.rs`，`ScriptedModel` 与 `MemTools` 立即返回，
  从入队到回合结束；这是**宿主侧**开销。

| 场景 | debug 中位 / p95 | release 中位 / p95 | n |
|---|---|---|---|
| 首 token（端到端，mock HTTP + pty） | 62.4 / 66.6 ms | 61.7 / 129.2 ms | 10 |
| 工具往返（端到端，1 工具 + 2 次模型调用） | 59.9 / 63.1 ms | 60.3 / 111.7 ms | 5 |
| 单次模型调用的整轮（进程内） | 55.7 / 59.0 ms | 56.0 / 64.6 ms | 100 |
| 工具往返整轮（进程内） | 83.8 / 89.8 ms | 83.9 / 103.7 ms | 100 |

**关键发现**：即使模型和工具都**立即**返回，进程内一轮也要 **~56 ms**，工具往返 ~84 ms，
release 与 debug 几乎一样。这不是模型或 HTTP 的开销，而是 **JSONL 的同步 fsync**：本仓库
一次 `append`（write + flush + fsync）中位 **4.0 ms**（见 §3），而一轮会写十几条记录
（`InputClaimed`/`TurnStarted`/`MessageAppended`/`TurnFinished`/…）。~56 ms / ~4 ms ≈ 14 次
fsync，与 ADR 0006 §15.2 的估算一致。这与机器负载无关（fsync 等磁盘）。

## 3. JSONL 与 SQLite

方法：`perf_session.rs`。append 用真实的 `SessionJournal::append`；`Session::open` 读一个
直接写盘、格式合法的 v1 文件（固定生成方式，见测试里的 `write_session`）；索引用
`Index::rebuild` 从零重建。`Session::open` 每档 5 次，取中位。

| 操作 | debug 中位 / p95 | release 中位 / p95 |
|---|---|---|
| append（write + flush + fsync，n=200） | 4.08 / 7.99 ms | **4.03 / 4.97 ms** |
| `Session::open` 1k 事件 | 9.76 / 14.24 ms | 0.81 / 0.85 ms |
| `Session::open` 10k 事件 | 93.3–173.6 / 280 ms | 8.43 / 8.64 ms |
| `Session::open` 100k 事件 | 981 / 1115 ms | 119.3 / 159.9 ms |
| `Index::rebuild` 10k 事件（10 文件） | 197.7 ms（19.8 µs/事件） | 63.1 ms（6.3 µs/事件） |
| `Index::rebuild` 100k 事件（10 文件） | 1400.3 ms（14.0 µs/事件） | 350.7 ms（3.5 µs/事件） |

读路径 release 约 **1.2 µs/事件**（100k 用 119 ms），线性；索引重建约 **3.5 µs/事件**。
append 的 fsync 是 latency 的主要来源，**读**不是。debug 的 10k 数字波动大（93–174 ms），
是负载导致的，故给出区间。

## 4. TUI 渲染（无虚拟化）

方法：`perf_render.rs`。在 `ratatui::TestBackend`（120 列 × 40 行）上构造 transcript 并
逐帧 `draw`。条目混合是固定的：每 20 条里有 3 条 ~2 KiB 的工具结果（含结尾 `ERROR:` 行）、
2 条中文、其余是 user/assistant/reasoning/tool 短条目；平均每条约 300 字节。每档
release 20 次（≥10k 时 5 次）、debug 20 次（≥10k 时 5 次）。

TUI **没有虚拟化**：`render::draw` 每帧重新换行并样式化**全部**条目
（`crates/radian-tui/README.md` 已写明）。因此帧耗时随条目数近似线性：

| 条目数 | debug 中位 / p95 | release 中位 / p95 |
|---|---|---|
| 100 | 2.09 / 2.15 ms | 0.25 / 0.32 ms |
| 500 | 5.86 / 6.00 ms | 0.79 / 1.94 ms |
| 1 000 | 10.62 / 10.86 ms | 1.47 / 1.61 ms |
| 2 000 | **20.49** / 21.63 ms | 2.84 / 3.15 ms |
| 5 000 | 49.02 / 53.45 ms | 7.01 / 7.65 ms |
| 10 000 | 102.4 / 106.3 ms | 13.91 / 14.87 ms |
| 20 000 | 218.8 / 242.2 ms | **27.65** / 36.43 ms |
| 50 000 | 532.6 / 568.2 ms | 76.01 / 88.19 ms |

**超过 16 ms/帧（60 fps 预算）的阈值**（按上表线性插值，同一混合、120×40）：

- **release：约 11 500 条目**（10k = 13.9 ms 过，20k = 27.7 ms 过）。
- **debug：约 1 550 条目**（1k = 10.6 ms 过，2k = 20.5 ms 过）。

即一个有 ~11k 条目的长会话在 release 下开始掉出 60 fps；debug 下 ~1.5k 条目就已掉出。
成本大致和**总字符数**（换行后的行数）成正比，不只和条目数有关；这里的长工具结果是主要
贡献者。宽终端会减少换行行数，从而降低单帧成本。

## 5. 内存上界

方法：外部测量单进程 RSS（不用 in-process `getrusage`，以免各档互相抬高峰值）。
`agent` 用 macOS `/usr/bin/time -l` 的 `maximum resident set size`；`radian-tui` 在
Python 的 pty 里启动、读到输入框后 kill，取 `resource.getrusage(RUSAGE_CHILDREN).ru_maxrss`。
release 构建。

| 进程 | 会话 | 峰值 RSS |
|---|---|---|
| `agent` | 全新（空会话） | 11.3 MB |
| `agent --resume` | 1 000 条 | 14.6 MB |
| `agent --resume` | 10 000 条 | 34.6 MB |
| `radian-tui` | 全新 | 11.5 MB |
| `radian-tui --resume` | 1 000 条 | 14.1 MB |
| `radian-tui --resume` | 10 000 条 | 38.1 MB |

约 **2.3–2.7 KB/条消息**（10k 比空会话多 ~23–27 MB）。10k 条已占用 ~35–38 MB，尚属温和；
按线性外推，100k 条会到 ~250 MB 量级——**未实测**（未测）。

## 发现（都带数据，未据此改任何产品代码）

1. **回合延迟由 fsync 主导**：进程内、模型与工具都立即返回时，一轮仍要 ~56 ms（release），
   工具往返 ~84 ms；`append` 本身 4.0 ms。交互式可用，但成批工具（每批十几条 append）
   会把延迟累加到几百毫秒。这是 ADR 0006 §15.2 已知代价的独立复核，量级一致。
2. **TUI 每帧重渲染全部 transcript**：release 下约 11.5k 条目、debug 下约 1.5k 条目即
   超过 16 ms/帧。这就是 README 里"暂无虚拟化"的可量化后果。
3. **`Session::open` 与索引重建都是线性且便宜**（1.2 µs/事件、3.5 µs/事件），不是瓶颈。
4. **启动 ~50–100 ms**，其中沙箱探针 ~25 ms；`--resume` 10k 在 release 下 ~50–160 ms。

## 局限 / 未测

- 负载 7–58 波动导致绝对值（尤其 p95）噪声大；未在空载机上重测。
- 只在这台 M4 / macOS 27 / APFS 上测；Linux、x86_64、其他文件系统未测。
- 无 criterion，纯 `std::time::Instant`；没有统计显著性检验，样本数见各表。
- **未测**：真实 provider 的首 token（本测量用本地 mock，排除网络）；`agent` 的 TUI
  之外的流式渲染；100k 条会话的 TUI 帧耗时与内存；虚拟化滚动（尚无实现）；多会话并发；
  索引增量更新（只测了从零重建）；`safe`/`full-access` 档位的启动差异；`--resume` 的
  端到端磁盘冷缓存情况。
- 首 token 的 pty 口径含最多 ~1 ms 的轮询量化；进程内口径不含 HTTP 与终端。

## 测量代码位置

- `crates/radian-session/tests/perf_session.rs`
- `crates/radian-runtime/tests/perf_runtime.rs`
- `crates/radian-tui/tests/perf_render.rs`
- `crates/radian-tui/tests/perf_startup.rs`
- `crates/agent-cli/tests/perf_agent.rs`

全部 `#[ignore]`；默认 `cargo test` 不运行它们，也不新增依赖。
