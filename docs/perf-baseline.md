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
（`--base-url` 指向死端口，不跑回合）。**沙箱探针包含在启动内**（`workspace` 档位；缺省是 `full`，无探针）。

| 场景 | debug 中位 / p95 | release 中位 / p95 | n |
|---|---|---|---|
| `radian-tui` 启动到输入框 | 97.0 / 104.4 ms | 98.4 / 150.3 ms | 10 |
| `radian --resume` 1k 条 | 109.2 / 113.4 ms | 89.7 / 96.2 ms | 5 |
| `radian --resume` 10k 条 | 529.9 / 561.8 ms | 161.0 / 170.8 ms | 5 |
| `agent` 启动到提示符 | 32.3 / 39.2 ms | 54.9 / 80.4 ms | 10 |
| `radian exec --resume` 1k 条 | 38.5 / 51.7 ms | 41.5 / 76.6 ms | 5 |
| `radian exec --resume` 10k 条 | 171.2 / 196.5 ms | 54.4 / 59.7 ms | 5 |
| `workspace` 沙箱探针 | 23.7 / 48.6 ms | 26.6 / 52.7 ms | 20 |

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

### 组提交之后（2026-10-16，ADR 0040）

同一个 `perf_runtime.rs`，同一台机器，只有同步点 fsync 之后（一轮 2–3 次，见
`radian-runtime/tests/group_commit.rs`）：

| 场景 | debug 中位 / p95 | release 中位 / p95 | n |
|---|---|---|---|
| 单次模型调用的整轮（进程内） | 20.5 / 60.0 ms | 16.7 / 26.3 ms | 100 |
| 工具往返整轮（进程内） | 44.8 / 92.7 ms | 26.1 / 40.8 ms | 100 |

测量时 `uptime` 的 1 分钟负载：release 55–76，debug 91（debug 的 p95 是负载尖峰）。相对本节
上半的 56 / 84 ms（release），一轮从约 14 次 fsync 降到 2–3 次；剩下的时间主要是每次 fsync
本身与进程内回合开销，不再随记录数线性叠加。release 的 16.7 → 26.1 ms 之差就是工具往返多
出的那一次 `ToolExecutionStarted` fsync（约 9 ms，含负载）。

## 3. JSONL 与 SQLite

方法：`perf_session.rs`。append 用真实的 `SessionJournal::append`；`Session::open` 读一个
直接写盘、格式合法的 v1 文件（固定生成方式，见测试里的 `write_session`）；索引用
`Index::rebuild` 从零重建。`Session::open` 每档 5 次，取中位。

| 操作 | debug 中位 / p95 | release 中位 / p95 |
|---|---|---|
| append（`write` + `flush`，非同步点记录，n=50，均值） | 0.211 ms | **0.025 ms** |
| `Session::open` 1k 事件 | 9.76 / 14.24 ms | 0.81 / 0.85 ms |
| `Session::open` 10k 事件 | 93.3–173.6 / 280 ms | 8.43 / 8.64 ms |
| `Session::open` 100k 事件 | 981 / 1115 ms | 119.3 / 159.9 ms |
| `Index::rebuild` 10k 事件（10 文件） | 197.7 ms（19.8 µs/事件） | 63.1 ms（6.3 µs/事件） |
| `Index::rebuild` 100k 事件（10 文件） | 1400.3 ms（14.0 µs/事件） | 350.7 ms（3.5 µs/事件） |

读路径 release 约 **1.2 µs/事件**（100k 用 119 ms），线性；索引重建约 **3.5 µs/事件**。
append 的 fsync 是 latency 的主要来源，**读**不是。debug 的 10k 数字波动大（93–174 ms），
是负载导致的，故给出区间。

**2026-10-16（ADR 0040 组提交）**：只有同步点（`SessionCreated`、`ToolExecutionStarted`、
`TurnFinished`/`TurnFailed`、替换提交/中止、`TaskSpawned`）fsync；第一行的非同步点
`append` 因此从约 4 ms 降到上表的约 0.03 ms。一次同步点的 append 仍付一次 fsync（约
4 ms，ADR 0006 §15.2 的测量），而一个回合只发生 2–3 次（见 §2 末）。

## 4. TUI 渲染（下表为虚拟化之前；之后见本节末）

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
| `radian exec --resume` | 1 000 条 | 14.6 MB |
| `radian exec --resume` | 10 000 条 | 34.6 MB |
| `radian-tui` | 全新 | 11.5 MB |
| `radian --resume` | 1 000 条 | 14.1 MB |
| `radian --resume` | 10 000 条 | 38.1 MB |

约 **2.3–2.7 KB/条消息**（10k 比空会话多 ~23–27 MB）。10k 条已占用 ~35–38 MB，尚属温和；
按线性外推，100k 条会到 ~250 MB 量级——**未实测**（未测）。

### 虚拟化之后（2026-10-13，dogfooding D9）

`render::draw` 现在只换行与样式化落在可见窗口里的条目，其余条目贡献缓存的行数（按宽度、
文本长度、interrupted 失效）；滚动位置与行数从 `u16` 改为 `usize`。同一个 `perf_render.rs`，
同一台机器，**测量时机器负载约 156（三个 pi 在编译），只看量级**：

| 条目数 | debug 中位 / p95 | release 中位 / p95 |
|---|---|---|
| 1 000 | 1.31 / 1.36 ms | 0.10 / 0.10 ms |
| 10 000 | 1.76 / 1.92 ms | 0.12 / 0.12 ms |
| 20 000 | 2.51 / 2.55 ms | 0.16 / 0.16 ms |
| 50 000 | 3.51 / 3.64 ms | 0.26 / 0.27 ms |

测到 50k 条目都没有超过 16 ms。剩下的线性部分是每帧对缓存行数求和。**未优化的一次性开销**：
`--resume` 一个大会话后的第一帧要量一遍所有条目（等于旧的一帧），之后不再重复。

## 发现（都带数据，未据此改任何产品代码）

1. **回合延迟由 fsync 主导**：进程内、模型与工具都立即返回时，一轮仍要 ~56 ms（release），
   工具往返 ~84 ms；`append` 本身 4.0 ms。交互式可用，但成批工具（每批十几条 append）
   会把延迟累加到几百毫秒。这是 ADR 0006 §15.2 已知代价的独立复核，量级一致。
   **ADR 0040 之后（组提交，2026-10-16）**：同一测量为一轮 **16.7 ms**、工具往返 **26.1 ms**
   （release），一轮只 fsync 2–3 次；见 §2 末。非同步点 `append` 不再 fsync（§3）。
2. **TUI 每帧重渲染全部 transcript**：release 下约 11.5k 条目、debug 下约 1.5k 条目即
   超过 16 ms/帧。这就是 README 里"暂无虚拟化"的可量化后果。**已修（2026-10-13，虚拟化，
   见 §4 末）。**
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
- `crates/radian-tui/tests/perf_render.rs`（§4 的帧耗时；§7 的 ADR 0023 Step 0 全套基线）
- `crates/radian-tui/tests/perf_startup.rs`
- `crates/agent-cli/tests/perf_agent.rs`
- `crates/radian-session/tests/perf_long_session.rs`（§6：open+history、RSS、索引重建）
- `crates/radian-session/tests/common/long_session.rs`（§6 的合成会话生成器，三处共用）
- `crates/agent-cli/tests/perf_long_resume.rs`（§6：resume 到第一个请求）

全部 `#[ignore]`；默认 `cargo test` 不运行它们。生成器只新增一个测试模块，没有新依赖。

## 6. 长会话 resume（Phase 5，只测量）

**状态：测量进行中——本节的 TUI 一项尚未完成（暂停时未做）；已有数字如下，未据此改任何产品代码。**

机器同上（M4 / macOS 27 / APFS，release）。**测量时 `uptime` 负载 73–150**（同时有多个
pi 在编译），所以绝对值（尤其 p95）噪声大，只读量级与增长趋势。

合成会话由 `crates/radian-session/tests/common/long_session.rs` 生成（三处 `#[path]` 共用）：
1k / 10k / 50k / 100k 条消息，混合 user、assistant（~2.4 KiB 文本 + ~1.6 KiB reasoning）、
工具调用 + 4 KiB 工具结果；另有一份含 **3 次已提交压缩**（ADR 0018/0019
`ContextCompacted` → `TranscriptReplaced` → 消息 → `TranscriptReplacementCommitted`）的版本。
压缩版 raw 文件只比 plain 多 600 条（3×201），但重建出的 live history 小很多。

| 文件 | raw 消息 | live history | 字节 |
|---|---|---|---|
| 1k plain / compacted | 1 000 / 1 600 | 1 000 / 451 | 1.6 / 1.8 MB |
| 10k plain / compacted | 10 000 / 10 600 | 10 000 / 2 701 | 16.3 / 16.5 MB |
| 50k plain / compacted | 50 000 / 50 600 | 50 000 / 12 701 | 81.8 / 81.9 MB |
| 100k plain / compacted | 100 000 / 100 600 | 100 000 / 25 201 | 163.7 / 163.8 MB |

### `Session::open` + `history()`（5 次中位；峰值 RSS 来自每档独立的子进程）

```sh
cargo test --release -p radian-session --test perf_long_session -- --ignored --nocapture --test-threads=1 perf_long_session_open
```

| 规模 | plain 中位 | plain 峰值 RSS | compacted 中位 | compacted 峰值 RSS |
|---|---|---|---|---|
| 1k | 2.2 ms | 13.5 MB | 3.5 ms | 13.5 MB |
| 10k | 57.0 ms | 81.1 MB | 34.2 ms | 66.1 MB |
| 50k | 250.0 ms | 415.9 MB | 201.4 ms | 308.5 MB |
| 100k | 488.7 ms | 818.8 MB | 338.8 ms | 683.0 MB |

- 时间随**文件字节数**近似线性（~3 µs/消息，~1.2 µs/KiB 量级；10k 的 57 ms 有负载尖峰）。
- **峰值 RSS 约 8 KB/消息**（100k plain 就到 **~819 MB**）；在这个混合里（每 5 条一个 4 KiB
  工具结果 + 长文本/推理）内存比 §5 的简单 10k 会话（34.6 MB）大得多。压缩版的文件同样大，
  但重建的 history 小，所以 RSS 低一些（100k compacted 683 MB vs 819 MB）——即**读文件的成本
  按 raw 大小，history 的内存按 live 大小**。

### 索引重建（`--list` 的后端）

```sh
cargo test --release -p radian-session --test perf_long_session -- --ignored --nocapture --test-threads=1 perf_long_index_rebuild
```

单文件：1k 5.5 ms · 10k 64.6 ms · 50k 823 ms · 100k **1877 ms**（5.5 → 18.8 µs/消息，
这条曲线上翘，可能是 SQLite/WAL 与负载；未单独求证）。

### `radian exec --resume <id> -p x`：启动到第一个请求

```sh
cargo test --release -p agent-cli --test perf_long_resume -- --ignored --nocapture --test-threads=1
```

mock 是一个裸 TCP listener：接受一次连接、读到请求头（`\r\n\r\n`）即记时（adapter 自带的进程内
mock 拒绝 >16 MiB 的请求体，50k+ 的 resume 超限，所以用裸 socket 才能在每一档看到请求）。

| 规模 | plain 中位 | compacted 中位 |
|---|---|---|
| 1k | 64.0 ms | 66.1 ms |
| 10k | 197.7 ms | 143.7 ms |
| 50k | 621.3 ms | 484.1 ms |
| 100k | **1526.9 ms** | **872.3 ms** |

100k plain 在 **1.5 s** 内都没能把第一个请求发出去；compacted 版（live history 25k）是 872 ms。
两者都要先把整个 raw 文件读进来，所以这是"读文件 + 重建 history + 序列化整个 history"的成本。

### 未完成 / 未测（暂停时）

- **`radian --resume` 到第一帧、以及首帧后 `Ctrl-Home` 滚动到顶的耗时**：未测（本片暂停
  在这里）。计划复用 `perf_startup.rs` 的 `libc::openpty` harness，`--freeze-draw` 无关；第一帧的
  判据是最后一条消息的标记，滚动判据是第一条消息的标记。
- 真实 provider 的首 token（本测量只在第一个请求发出即停）；`--resume` 的磁盘冷缓存；
  100k 条的 TUI 帧耗时与滚动；多会话并发。
- 本节数字只在这台机器/负载下成立；没有统计显著性检验（每档 5 次中位）。

## 7. ADR 0023 Step 0 基线（重构前，main 6da63c2）

ADR 0023 的 Step 0：在 `radian-tui/src/{app.rs,main.rs}` 重构（PR ①）**之前**量一次，作为对照。
**不改产品代码**；测量代码只有 `crates/radian-tui/tests/perf_render.rs`（全部 `#[ignore]`）。

```sh
cargo test --release -p radian-tui --test perf_render -- --ignored --nocapture --test-threads=1
```

**负载**：测量期间本机 load average **195–252**（同一台机器上还有别的 pi 在编译），远高于正常；
下面每个数字都在这条负载下取得，绝对值（尤其 p95/max 与 50k 的 resume）噪声很大。同一命令跑了
两遍，两遍一致的写一个数，差异大的写区间。

**方法**：帧耗时用 ratatui `TestBackend` 120×40（只驱动公开 API：`App`、`render::draw`），每档
20 次取 p50/p95/max；重排/滚动/流式/内存各 5 次或更多次。空闲重绘与 `--resume` 用真实 pty
（`libc::openpty`，24×100），检测轮询 1 ms，所以数字向上取整最多 ~1 ms。**`TestBackend` 量的是
我们的渲染成本，不含终端 I/O 延迟、终端解析与合成器；pty 量的是"字节到达"，不是真实终端的绘制
延迟。** 真实终端上的体感未测。

### 空闲重绘（pty，5 秒观测窗口）

现状的循环在 `main.rs`：每 50 ms poll 一次输入，然后**总是**画一帧。

| | 5.0 s 内 | 折合 |
|---|---|---|
| 帧（以每帧的 `?25h` 光标显示标记计） | 96 | **19.1 帧/秒** |
| pty 上收到的字节 | 3072 B | **32 B/帧** |

即：屏幕完全静止时也以约 20 fps 重绘，每帧约 32 B（样式复位 + 显示光标 + 光标定位）。

### `render::draw` 稳态帧（TestBackend 120×40，20 次）

| 条目 | p50 | p95 | max |
|---|---|---|---|
| 1 000 | 0.10 ms | 0.10–0.11 ms | 0.10–0.11 ms |
| 10 000 | 0.13–0.14 ms | 0.23–0.33 ms | 0.23–0.33 ms |
| 50 000 | 0.27–0.44 ms | 0.33–0.71 ms | 0.33–0.71 ms |

所有档位都远低于 16 ms/帧；50000 条的稳态帧仍是**亚毫秒**（虚拟化只包可见条目，其余走缓存行数）。

### 宽度变化（120 ↔ 80）后的第一帧

宽度一改，按旧宽度缓存的换行全部失效，**新宽度的第一帧要重新换行整个 transcript**。

| 条目 | 稳态@120 | 第一帧@80 p50 | 第一帧@80 p95 | 第一帧@120 p50 | 稳态@80 |
|---|---|---|---|---|---|
| 1 000 | 0.11–0.26 ms | 1.25–1.39 ms | 1.89–1.90 ms | 1.41–1.43 ms | 0.08 ms |
| 10 000 | 0.13–0.20 ms | 14.6–16.5 ms | 15.7–21.1 ms | 13.7–18.7 ms | 0.12–0.15 ms |
| 50 000 | 0.27–0.29 ms | **66.9–78.1 ms** | **73.9–81.6 ms** | 67.0–69.2 ms | 0.25–0.26 ms |

50k 条的一帧重排要 **~70 ms**（超过 16 ms 预算约 4 倍），而它前后的稳态帧只要 ~0.27 ms。
两个方向（变窄、变宽）成本同量级。

### 50 000 条的滚动

`PageUp` ×100（每次按键 + 显示该键效果的那一帧）：总计 **28.4–36.1 ms**，
每键 p50 **0.27–0.28 ms**、p95 0.31–0.69 ms。`Ctrl-Home` 到顶（按键 + 帧）p50
**0.25–0.63 ms**。两者都便宜，因为行数缓存已热，滚动只需新换行刚露出的那几十行。

### 流式（4 个 App 同时流，事件循环的 50 ms tick 模型）

场景是"每 1 ms 一个 delta、持续 5 s" = 5000 个 delta；`main.rs` 每 50 ms 把通道里的 delta 一次
排空再画一帧，所以一帧看到 ~50 个 delta。本测量按此模型：4 个 App 各自每轮吃 50 个 delta，只画
聚焦的那个，共 100 轮。**harness 不节流**（把这 5000 个 delta 尽快喂进去），因此墙钟只有
0.06–0.09 s、实际速率 5.4–8.3 万 delta/s；每帧的工作量由 50 delta/帧 决定，与节流无关，帧耗时
和按键延迟就是结论。

| | p50 | p95 | max |
|---|---|---|---|
| 单帧 | 0.57–0.70 ms | 1.15–1.87 ms | 1.64–2.04 ms |
| 按键（模拟 `'x'` 的 `on_key`） | ~0.000 ms | 0.000–0.001 ms | 0.001–0.004 ms |

流式负载下帧仍远低于 16 ms；按键应用是微秒级。

### 内存（50 000 条目的 `App`，fresh 进程的 `getrusage` 峰值 RSS）

| | RSS |
|---|---|
| 测试进程基线 | 6 MiB |
| `App` + 50k `Entry` 构建后 | 42 MiB（**+36 MiB**） |
| 再画第一帧后 | 44 MiB（再 +1 MiB） |

（`ru_maxrss` 是进程高水位，所以本测量 re-exec 测试二进制、只跑这一个测试，否则会读到同一进程里
更早测试的峰值。）

### `radian --resume` 到第一帧，与首帧后 `Ctrl-Home`

用 `pi/longsess` 的合成长会话生成器（`crates/radian-session/tests/common/long_session.rs`，经
`#[path]` 共用），1k/10k/50k 条消息、无压缩。`--resume` 的历史在循环之前渲染，所以底部
`resumed session …` 通知出现 = 第一帧；`Ctrl-Home` 的判据是顶部第一条工具调用的文本出现。

| 消息数 | 文件 | 到第一帧 p50 | 到第一帧 p95 | `Ctrl-Home` 到顶 p50 |
|---|---|---|---|---|
| 1 000 | 1 MB | 104–109 ms | 113–115 ms | 1.5–1.6 ms |
| 10 000 | 15 MB | 294–333 ms | 334–790 ms | 1.6 ms |
| 50 000 | 78 MB | **2208–4691 ms** | 2506–7940 ms | 1.5–1.6 ms |

50k 条的 `--resume` 第一帧要 **2.2–4.7 s**（两遍差异就是负载噪声）。`Ctrl-Home` 到顶仍是 ~1.5 ms。

### 结论（只写上面量到的）

- 稳态帧、滚动、流式：50k 条下都在亚毫秒~毫秒级，远低于 16 ms。
- **两处整 transcript 的全量重排**：宽度变化后的第一帧（50k **~70 ms**）、以及 `--resume` 的第一帧
  （50k **2.2–4.7 s**，含读 78 MB 文件 + 重建 history + 每个条目首次换行）。这两处是 Step 1 的
  明显目标。
- 空闲重绘 **~19 fps**、每帧 32 B：现状每秒无条件重绘约 20 次。
- 50k 条目的 `App` 常驻约 **36 MiB**。

### 局限 / 未测

- 真实终端（Terminal.app/iTerm2/tmux/ssh）上的绘制延迟与滚动体感：`TestBackend`/pty 测不出。
- 负载 195–252 下的数字；没有统计显著性检验，也没有跨机器对比。
- 多**进程**（而非本测量的多 `App`）同时流式；鼠标、粘贴、搜索、审批等路径未测。
- `--resume` 的冷磁盘缓存；100k 条。

## 7b. ADR 0023 Step 1 PR ② 之后（事件驱动循环与脏通知）

PR ② 把 `radian-tui` 的循环换成**一条 `UiEvent` 通道 + 阻塞 `recv`**：流式 delta 不再进通道
（进 `SessionProjection` 的累积缓冲，用 `dirty` 标志唤醒），信号、driver 结束、命令结果都是事件，
终端输入由专门的输入线程转发；每轮最多处理 `MAX_FACTS_PER_TURN = 64` 条事实，然后若脏才画一帧。

**负载**：测量期间本机 load average **76–95**（同一台机器上还有别的 pi 在编译）；§7 的基线是
195–252。两次数值因此不可逐项严格对比，但空闲重绘的结论不受负载影响（它是 0 与非 0 的区别）。

命令（与 §7 相同）：

```sh
cargo test --release -p radian-tui --test perf_render -- --ignored --nocapture --test-threads=1
```

### 空闲重绘（pty，5 秒观测窗口）——本 PR 的验收门槛

| | 5.0 s 内 | 折合 |
|---|---|---|
| **PR ② 之前（§7）** | 96 帧 | **19.1 帧/秒**，32 B/帧 |
| **PR ② 之后** | **0 帧** | **0 帧/秒，0 B** |

空闲零重绘成立：没有事件、没有随时间变化的显示（状态栏、通知、搜索框、预算行都是静态文本，
所以循环用阻塞 `recv`，没有 `recv_timeout`）时不画。回归测试是
`pty_an_idle_screen_is_not_redrawn`（同一计数法，2 秒窗口断言帧数不变）。

### 其余档位（PR ② 之后重跑，供对照）

| 档位 | §7 基线 | PR ② 之后 |
|---|---|---|
| `render::draw` 稳态 1k / 10k / 50k（TestBackend 120×40） | 0.10 / 0.13 / 0.27 ms（p50） | 0.10 / 0.13 / **0.49 ms**（p50；p95 1.12 ms） |
| 宽度变化第一帧 50k | 66.9–78.1 ms（p50） | 59.9 ms（p50，p95 141.6 ms） |
| 50k 滚动 PageUp×100 | 28.4–36.1 ms 总计 | 32.1 ms 总计（p50 0.29 ms/键） |
| 流式（4 app，50 delta/轮，100 轮） | 单帧 p50 0.57–0.70 ms | 单帧 p50 **0.56 ms**（p95 1.11 ms）；按键 p50 0.000 ms |
| 50k 条目 RSS | +36 MiB | +36 MiB |
| `--resume` 1k / 10k / 50k 到第一帧 | 104–109 / 294–333 / 2208–4691 ms | 41 / 219 / 1983 ms（负载更低，未据此宣称改进） |

流式一档现在走的是新路径（delta → `SessionProjection::push_delta` → `snapshot` →
`App::update(Input::Stream)`），也就是循环实际做的事。

### 绘制公平（本 PR 的验收门槛，pty 测试）

`pty_a_flooding_producer_still_draws_and_answers_keys`：测试接缝 `--flood-facts 20000` 以最快速度
灌 20 000 条事实（真实生产者受 turn 结构约束，只有这个接缝不受约束，规则才可测），循环每轮只吃
64 条，所以 2 秒窗口里帧数仍在增长（断言 ≥100 帧），按键在有界时间内被处理。去掉每轮上限的变异
会让整批一次排空（2 秒窗口只剩 1 帧），该测试抓住。

### 唤醒顺序（确定性测试，不用 sleep）

`crates/radian-tui/src/projection.rs` 的单元测试：生产者在 UI **清标志之前**再更新不产生多余的
`Dirty`（`a_delta_wakes_the_ui_once_until_it_reads`）；在 UI **读取之后**更新必定再唤醒一次
（`an_update_after_the_read_wakes_the_ui_again`）。顺序契约是 `Release`/`Acquire`，没有 `Relaxed`。

### 局限（与 §7 相同，另加）

- 真实终端上的绘制延迟与滚动体感仍未测（`TestBackend`/pty 测不出）。
- 本节的绝对值取自 load 76–95 的一次运行；空闲帧数（0）与公平性（帧数仍增长）是结构性的结论。
- `MAX_FACTS_PER_TURN = 64` 是按"一轮的活接近旧 50 ms tick（~50 个 delta）"选的，由公平性测试
  验证；它不是从延迟数据反推的最优值。

### 重构后对照（ADR 0023 Step 1 · PR ④，main 677ca35）

同一命令、同一台机器（负载 **110–157**，比 Step 0 的 195–252 低）。`App` 的会话事实已搬进
`radian-host` 的 `SessionProjection`，`render.rs` 读投影；`main.rs` 改为调用
`radian_host::open_session`。

| 项目 | Step 0（重构前） | PR ④ 后 |
|---|---|---|
| 空闲重绘（pty 5 s） | 96 帧 / 19.1 帧每秒，3072 B | **0 帧 / 0 B**（PR ② 的零空闲重绘） |
| 稳态帧 1k / 10k / 50k（p50） | 0.10 / 0.13–0.14 / 0.27–0.44 ms | 0.10 / 0.13 / 0.27 ms |
| 改宽首帧 50k（p50） | 66.9–78.1 ms | **59.9–60.0 ms** |
| 改宽首帧 10k（p50） | 14.6–16.5 ms | 11.4–11.5 ms |
| 50k PageUp ×100 总计 | 28.4–36.1 ms | 29.4 ms |
| 50k Ctrl-Home 到顶（p50） | 0.25–0.63 ms | 0.26 ms |
| 流式帧 p50 / p95 | 0.57–0.70 / 1.15–1.87 ms | 0.62 / 0.94 ms |
| 50k `App` 峰值 RSS | +36 MiB（6 → 42） | +36 MiB（6 → 42） |
| `--resume` 首帧 1k / 10k / 50k | 104–109 / 294–333 / 2208–4691 ms | 51 / 270 / 1760 ms |

**结论**：没有明显退化。稳态帧、滚动、流式、内存与 Step 0 同量级（差异在负载噪声内）；改宽首帧
反而略快，`--resume` 首帧也更快（负载更低）。**唯一显著变化是空闲重绘 19.1 → 0 帧/秒**，这是
PR ② 的零空闲重绘（ADR 0023 U2），不是本片；本片把状态栏改为读投影后，空闲时同样零重绘。

本片的每次 `Dirty` 通知（含只有 usage/budget/tier/MCP 这类非流式事实的更新）都会画一帧
（`apply_dirty` 的单元测试钉住），所以零重绘只在"完全没有事件"时成立。

## 鼠标滚轮（PR mouse，ADR 0023 U2 方案 B）

`tui.mouse`（默认 on）让终端上报按键与滚轮（`CSI ?1000h` + SGR `CSI ?1006h`），滚轮不再被终端
翻译成方向键。分类上它属于**终端输入**（ADR 0023 U2 的“保序”类）：一格滚轮是一次输入，会画一帧
（滚动改变了内容，正是该画的）。**指针移动不产生帧**：`?1002h`/`?1003h`（any-motion）**不开**，
crossterm 的 `MouseEventKind::Moved` 在 `events::input_event` 里被丢掉，根本进不了通道（单元测试
`events::tests::only_a_wheel_notch_is_mouse_input`）。因此**空闲零重绘仍是 0**：没有滚轮、没有按键时，
没有事件，也就不画。

实测门檻沿用 `pty_an_idle_screen_is_not_redrawn` 与 `pty_a_mouse_move_is_dropped`：前者证明空闲 5 秒
零帧，后者证明指针移动/点击不消耗输入（输入框的草稿不变、且输出里没有 `?1002h`/`?1003h`）。

## 块模型（ADR 0028 第 1 片 · R1/R7 门槛）

条目 → 块 → 行的显示模型（`blocks.rs`）替换了"整条纯文本折行"。测量命令与 §7 同：
`cargo test --release -p radian-tui --test perf_render -- --ignored --nocapture --test-threads=1`。
本次运行的机器负载 **115–230**（基线各节是 0.4–157），绝对数字偏悲观。

| 项目 | 基线（PR ④） | 块模型 |
|---|---|---|
| 稳态帧 50k（p50） | 0.27 ms | **0.25–0.46 ms** |
| 稳态帧 10k（p50） | 0.13 ms | 0.13 ms |
| 改宽首帧 50k（p50） | 59.9–78.1 ms | **27.3 ms**（p95 32.1） |
| 改宽首帧 10k（p50） | 11.4–16.5 ms | **6.2 ms**（p95 7.5） |
| 50k PageUp ×100 总计 | 29.4 ms | **27.4 ms**（每键 p50 0.26 ms） |
| 50k Ctrl-Home 到顶 | 0.26 ms | 0.25 ms |
| 流式帧 p50 / p95 | 0.62 / 0.94 ms | **0.73 / 1.26 ms** |

**结论**：R7 的两个门槛都过——常规一帧 < 1 ms（50k 稳态 0.25–0.46 ms），全局重排后的第一帧
< 100 ms（改宽首帧 50k 27.3 ms）。相对 PR ④ 基线还更快，原因是本轮把"量每一行"的路径改成
**只数不建**（`wrap_line_count` 与 `code_line_count`）：改宽首帧不再为每个条目分配行字符串，
只是扫描计数；只有真正可见的条目才 `layout` 出 `BodyLine`。流式一帧的成本仍与正在写入的
条目长度同阶（增量解析复用已闭合的块，但布局仍按该条目全量重排，与旧折行路径相同量级）。

**全局折叠切换的第一帧**（R7 要求 < 100 ms，`perf_render_reasoning_toggle`；本次运行负载
较高，50k 的稳态帧也涨到 2.49 ms）：

| 条目 | 折叠首帧 | 再展开首帧 | 之后的稳态帧 |
|---|---|---|---|
| 1 000 | 0.65 ms | 0.65 ms | 0.17 ms |
| 10 000 | 5.00 ms | 5.06 ms | 0.19 ms |
| 50 000 | **34.2 ms** | 55.3 ms | 2.49 ms |

折叠状态是块/布局缓存键的一部分，所以切换会让所有条目的行数失效并重数一遍——与改宽首帧同一
量级（只数不建），远低于 100 ms 门槛。

## `find` 搜索 5 万条目（review-fixes 片）

`find.rs` 的搜索不是逐帧测量：`ctrl-f` 打开后每敲一个字符都对**全部条目**重算一次匹配
（`compute` 对每条目做 wrap，5 万次分配）。它的计时**不是断言、而是 `#[ignore]` 基准**，与
`perf_render.rs` 同一条规则：debug 下的绝对值受负载支配（外部审查在 load 200+ 时观察到
2.59 s，超过旧的 2 s 上限）。默认测试只保留正确性（找到那一条、状态栏 `match 1/1`），
**不碰墙钟**；一条复杂度回归由基准里的 5k→50k 比值与 2 s 绝对值一起看。

```sh
cargo test -p radian-tui --test find a_search_over_fifty_thousand_entries_is_fast -- --ignored --nocapture
```

**负载**：本机 load average 119/98/96（同一台机器上还有别的 pi 在编译）。

| 场景 | 5k 校准 | 50k | 比值 |
|---|---|---|---|
| 一次 `ctrl-f` + 输入 `needle`（debug） | 43.8 ms | 1238.9 ms | 28.3× |

比值明显高于线性的 10×（5 万次分配的缓存/分配器压力），所以**比值也不能当硬界**；它只是让人
在基准输出里一眼看出量级。绝对上限 2 s 只在机器空闲时才有意义（`--ignored` 是人为选择时机）。

## Mermaid 图（ADR 0028 第 2 片 · R8）

闭合的 ```mermaid 块在 `blocks.rs` 里按 `(source, width)` 渲染并缓存（键含宽度），失败回退到
源码 + 一行弱色原因。测量命令与 §7 同：
`cargo test --release -p radian-tui --test perf_render -- --ignored --nocapture --test-threads=1`。

本次运行的机器负载高于基线（绝对数字偏悲观）。perf 语料是混合内容，**不含** mermaid 块，
所以数字与上一节同量级；mermaid 只影响含图的条目，而图一旦渲染就进缓存（同一 `(width,
text_len)` 不重渲）。

| 项目 | 块模型（上一节） | 本次（含 mermaid 接入） |
|---|---|---|
| 稳态帧 50k（p50） | 0.25–0.46 ms | **0.71 ms**（p95 1.59） |
| 改宽首帧 50k（p50） | 27.3 ms | **30.9–32.6 ms**（p95 59–63） |
| 50k 折叠首帧 | 34.2 ms | **36.9 ms** |
| 50k PageUp ×100 每键 p50 | 0.26 ms | **0.42 ms** |

**结论**：R7 门槛仍过——常规一帧 < 1 ms（50k 0.71 ms，p95 1.59 ms < 30 ms），全局重排
（改宽、折叠切换）后的第一帧 < 100 ms（30.9 / 36.9 ms）。语料不含 mermaid，所以这一节主要
证明接入没有让既有的每帧路径退化；含图的条目首帧多一次渲染，之后命中缓存。

**二进制体积**：`cargo build --release -p radian-tui --bin radian`，接入前 23,993,056 字节，
接入后 **24,169,680 字节**（+176,624，+0.74%）。

**渲染器已换（ADR 0042，2026-10-16）**：上面的数字与体积是旧渲染器那一版的，
此后渲染器换成 vendored 的 `third_party/grok-mermaid`（grok-build 的 `mermaid.rs`：Sugiyama 布局，
多支持 subgraph / 4 参与者时序图）。接入点仍在 `blocks.rs` 的 `(source, width)` 缓存里，失败仍是
源码 + 一行原因，所以这里的结论（接入不退化每帧路径）不变；**具体数字与体积要按新渲染器另测**。

## 工具卡片（ADR 0028 第 2 片 a · R4/R7 门槛）

工具卡片（数据、按名注册的渲染器、`tui.display.tool_output` 折叠 + Ctrl-O）加在块模型之上。
同一台机器，本次运行负载 **148–155**（比 §7 各节高，绝对值偏悲观）。

| 项目 | 块模型（上一片） | 工具卡片 |
|---|---|---|
| 稳态帧 50k（p50） | 0.25–0.46 ms | **0.39–0.40 ms** |
| 稳态帧 10k（p50） | 0.13 ms | 0.21 ms |
| 改宽首帧 50k（p50） | 27.3 ms | **29.4 ms**（p95 30.5） |
| 改宽首帧 10k（p50） | 6.2 ms | 6.0 ms |
| 折叠切换首帧 50k | 34.2 ms | **31.0 ms**（p95 见上；仍 << 100 ms） |

**结论**：R7 门槛不退化——常规一帧 50k 0.39 ms（< 1 ms），全局重排/折叠后的第一帧 50k 29–31 ms
（< 100 ms）。工具卡片的额外成本只落在工具条目上：计数与布局只对带卡片的条目多做一次
「选行 + 折行」；折叠仍然只数不建（`line_count` 走同一个 `body_plan` 但用 `wrap_line_count`）。

## 工具卡片渲染器（ADR 0028 第 2 片 b · R4/R7 门槛）

本片给 `patch`/`write`/`git_diff`/`git_status`/`list`/`ask_user`/记忆工具各加一个纯函数渲染器，
并把 diff 的四种行角色接进主题。测法与 §7 同（本次机器负载 **110–265**，绝对值偏悲观）。

| 项目 | 第 2 片 a | 第 2 片 b |
|---|---|---|
| 稳态帧 50k（p50） | 0.39–0.40 ms | **0.38–0.40 ms** |
| 稳态帧 10k（p50） | 0.21 ms | 0.20–0.21 ms |
| 改宽首帧 50k（p50） | 29.4 ms | **31.7 ms**（p95 32.6） |
| 改宽首帧 10k（p50） | 6.0 ms | 6.4 ms |

**结论**：R7 门槛不退化——常规一帧 50k 0.38 ms（< 1 ms），全局重排后的第一帧 50k 31.7 ms
（< 100 ms）。渲染器都是按名查表的纯函数，成本只在有卡片的条目上；`patch`/`write` 的正文由
调用参数合成，因此条目文本多一份内容（journal 本就有），但计数路径（`line_count`）仍只数不建。

## 压缩摘要与搜索自动展开（ADR 0028 第 4 片 · R5/R6/R7 门槛）

本片让压缩摘要折叠成一行（`EntryKind::Compaction`）、让 Ctrl-F 匹配**完整内容**（折叠的 thinking /
工具正文 / 摘要都搜得到）并在命中时只展开那一个条目（按条目的折叠覆盖，进布局缓存键）。测法与 §7 同；
本次运行负载 **243–299**（基线各节 0.4–157），绝对值偏悲观。

| 项目 | 第 2 片 b | 第 4 片 |
|---|---|---|
| 稳态帧 50k（p50） | 0.38–0.40 ms | **0.52–0.53 ms** |
| 稳态帧 10k（p50） | 0.20–0.21 ms | 0.22–0.23 ms |
| 改宽首帧 50k（p50） | 31.7 ms | **45.0 ms**（p95 51.1；@120 方向 53.0） |
| 50k 一次 `ctrl-f` + 输入 `needle`（debug，`#[ignore]` 基准） | 1238.9 ms（load 119） | **983.1 ms**（load 299；5k 校准 44.4 ms，比值 22.2×） |

**结论**：R7 门槛不退化——常规一帧 50k 0.52 ms（< 1 ms），全局重排后的第一帧 50k 45 ms（< 100 ms）；
50k 的搜索基准在更高的负载下反而更快（搜索几何与渲染几何共用 `body_text_lines`，本片没有给搜索
路径加新的分配）。按条目的折叠覆盖只在命中时写一个字段并让该条目的行数缓存失效（键含覆盖），
其余条目不受影响。
## Markdown（ADR 0028 第 3 片 · R2/R7）

Markdown 语料由 `perf_render.rs` 的 `markdown_entries(n)` 生成（不是大文件）：每 10 条里 6 条是
Assistant Markdown（标题、带行内样式的段落与链接、嵌套列表、表格、代码块），其余是用户提问、
thinking 和工具结果。命令与 §7 同。

| 项目 | 1k | 10k | 50k |
|---|---|---|---|
| 稳态帧 p50 | 0.11–0.32 ms | 0.20–0.26 ms | **0.59–0.84 ms**（p95 0.71–1.97） |
| 全局折叠首帧（Ctrl-T） | 0.42–0.61 ms | 3.6–5.4 ms | **24.0–27.7 ms** |

**结论**：R7 两个门槛都过——稳态帧 < 1 ms（50k 0.84 ms 是本次负载下的上沿），折叠首帧
< 100 ms（24–28 ms）。关键在于 Assistant 的 Markdown **段落行**按 `(width, text_len)` 缓存
（`Entry.md_cache`）：折叠切换会重数每个条目，但不重新解析 Markdown，所以首帧与块模型同一量级。

**`perf_tui_resume` 的失败与本片无关（已核实）**：它在**本片之前的 main（`5628737`，合并
mermaid 后）**上同样超时，而且连 warm-up（1000 条）都超时。用同样的 pty 探针（20 s 后看原始
输出）在两边都看到：应用**正常渲染**（transcript 在底部、状态栏正常），但 `resumed session …`
这行属于 **startup 屏**，它排在 transcript 顶部；resume 时视图 `follow` 到底部，所以它在首帧
**滚出屏幕**，测试等的那个字符串永远不出现。也就是说这是 startup 屏在 resume 长历史下的可见性
问题（或测试该换一个判据），不是 Markdown 的性能回归。本片没有改它。

### `perf_tui_resume` 判据修正与两组对照（markdown-4）

`perf_tui_resume` 的判据在 ui-r2 之后失效（不是本片）：它等的 `resumed session …` 被 ui-r2 移进
**startup 屏**，而 resume 跟随到底部，startup 屏首帧就滚出屏幕——所以它在**本片之前的 main 上
也超时**。判据已改成首帧一定画出的状态栏 `context budget off`；Ctrl-Home 的判据从原始工具参数
改成工具卡片标题 `-> read: file.txt`（tools 片改了渲染）。两个判据都仍然测到首帧/滚动完成。

同一台机器、相近负载下分别测（`--ignored --nocapture --test-threads=1`，5 次取样）：

| 语料 | main `a6a9cce` 首帧 p50 | pi/markdown `f7d4f41` 首帧 p50 | 比值 |
|---|---|---|---|
| 1 000 条 | 56 ms | 59 ms | 1.05× |
| 10 000 条 | 1 235 ms | 1 270 ms | 1.03× |
| 50 000 条 | 18 949 ms | 12 887 ms | **0.68×** |

**结论**：Markdown 没有让 resume 首帧变慢（50k 反而更快，10k/1k 在噪声内），远低于 1.5× 的
修修门槛，所以不做懒测量/尺寸上限。两个绝对值都由"resume 时首次测量全部条目"主导，与本片无关。

## Resume 首帧：`push_message` 的 O(n²)（resume-perf A）

`--resume` 到首帧的耗时分段（`RADIAN_TRACE_PHASES=1` 的环境变量仪表；方法见下）。50k 条消息：

| 段 | main `914d549` | 本片 `b55cab2` |
|---|---|---|
| `open/journal`（`SessionJournal::resume` 扫文件） | ~1.3 s | ~1.3 s |
| `recover`（解析 + replay + recover） | ~1.1 s | ~1.1 s |
| `open_session` 其余 | ~0.8 s | ~0.8 s |
| `history-read`（host 再开一次 `Session::open` + `history()`） | ~1.3 s | ~1.3 s |
| **`entries-built`（`App::push_message` 循环）** | **8.8–12.5 s** | **~0.10 s** |
| `line-counts`（首帧给全部 66672 条目算行数） | 1.8–3.8 s | 1.8–3.8 s |
| 首帧合计（`perf_tui_resume` p50） | 18 187 ms | 7 272–9 483 ms |

**原因**：`push_message` 每条消息都执行 `self.entries.retain(|e| !e.live)`，对已 6.6 万条的
transcript 全扫一次 → O(n²)。**修法**：`App` 维护 `live_count`（`set_live` 新建 live 条目时 +1，
`push_message` 丢弃后清零），只有 `live_count > 0` 才扫。live 条目**不一定在尾部**（压缩提示可能排在
它后面），所以用计数而不是尾部扫描。**目标与结果**：50k 首帧 ≤ 7 s；同机（负载很高，load ~400，
绝对值偏悲观）对照 main：1k 84 → 79–147 ms（噪声内）、10k 2 065 → 1 348–1 512 ms、
50k 18 187 → 7 272–9 483 ms。`entries-built` 从 8.8–12.5 s 回到 ~0.1 s。

**不依赖墙钟的测试与变异**（`crates/radian-tui/src/app.rs` 的单元测试）：`LIVE_SCANS` 统计
`push_message` 走过的条目数。`push_message_does_not_walk_the_transcript_without_a_live_entry`：
无 live 条目时该计数不增长。变异（恢复无条件 `retain`）→ 该测试失败。另两条覆盖「有 live 条目时
会扫」和「live 条目不在尾部时仍被丢弃、其后的 notice 保留」。

## Syntax highlighting（ADR 0028 第 5 片 · R2）

`syntect` 5.3（`default-features = false`，纯 Rust `regex-fancy`；syntect 的默认语法打包集）。
测量用 `cargo test --release -p radian-tui`（debug 构建下解析慢 10–30×，不作为数据）。

| 项目 | 值 |
|---|---|
| `SyntaxSet` 加载（反序列化合并 dump：默认 75 语法 + vendored TS/TSX/TOML） | **≈ 1.1 ms**（进程一次，`OnceLock`） |
| 解析 200 KB 的 TypeScript YAML（**只在重新生成 dump 时**，不再是构建期每次都做） | ≈ 5.5 s/文件（`yaml-rust` + syntect 逐条 pattern 的正则校验；见下一节） |
| 500 行 Rust 代码块首次高亮（含语法正则的惰性编译） | **≈ 125–285 ms** |
| 同一块之后的高亮（正则已编译） | **≈ 130–270 ms**（5 次取样；每 1000 行 ≈ 0.22 s） |
| TypeScript 首次解析（`fancy-regex` 编译该语法的全部正则，进程一次） | **≈ 380 ms**；之后 ≈ 0.3 ms |
| TOML 首次解析 | ≈ 0.6 ms |
| 语言表外 / 未闭合 / `highlight = false` | 0（不高亮） |

高亮结果按 `(width, text_len, highlight)` 缓存在 `Entry.syntax_cache`（角色，不是颜色，主题切换不失效），
所以稳态帧不重新解析。

**R7（50 000 条 Markdown 语料，每 10 条 6 条 Assistant、每条一个 ` ```rust ` 代码块）**，与同一台机器上
**本片之前的 main `54534cb`** 对照（都 `--release`）：

| 度量 | main `54534cb` | 本片 HEAD | 说明 |
|---|---|---|---|
| 稳态帧 p50 | 0.55 ms | **≈ 0.7 ms** | 高亮命中缓存；差异在噪声内 |
| 全局折叠首帧（50k） | 282 / 129 ms | **166 / 120 ms** | 本片不慢于 main；文档里早先的 24–28 ms 是别的机器/负载下的旧值 |

**二进制体积（release，含调试符号，`radian`）**：24 837 872 B → **25 865 344 B**，**+1 027 472 B
（≈ +0.98 MiB，+4.1%）**。其中前一版（只有默认集）是 25 758 480 B；vendored TS/TSX/TOML 与它们并入后的
dump 再加 **+106 864 B**（≈ 104 KiB）。合计是 syntect + `fancy-regex` + `regex` + `flate2`/`bincode` +
合并后的语法 dump（约 431 KB）的代价。手工精简默认集未采用（跨语法上下文引用会在 build 时越界）；TS/TOML
是原样 vendor 到 `third_party/syntaxes/`（见其 `RADIAN-CHANGES.md`）。

**结论**：R7 不退化；首次高亮一个很大的代码块会有百毫秒级的一次性开销（缓存后不再付），这是纯 Rust 正则
后端（`fancy-regex`）的代价，记为已知取舍。

### Resume 首帧：只解析一次（resume-perf B）

A 修掉 `push_message` 的 O(n²) 后，启动仍把同一份 JSONL 读 4 次。B 让它只解析一次
（`SessionJournal::resume_parsed` 复用宿主已解析的 `Session`；`Driver::open_with_slot_parsed`
的 `recover` 复用它；`LocalSession::history` 把已恢复的 transcript 交给 TUI，`main.rs` 不再
`Session::open`）。只有写出 replacement abort（改变磁盘上的 fold）时驱动才重新解析。50k：

| 段 | A（main `7ec6f62`） | B（本片） |
|---|---|---|
| `open/parse-once` | 扫描 + 解析 3 次（`journal/scan` + `journal/parse` + `recover/open`） | **~0.85 s（一次）** |
| `open/journal` | ~1.3–2.0 s | ~0（复用解析） |
| `recover/open` | ~1.0–1.5 s | ~0.14 s（只建 records） |
| `history-read`（宿主再开一次） | ~1.4–1.5 s | ~0（用已恢复的 history） |
| `entries-built` | ~0.1 s | ~0.15 s |
| `line-counts`（首帧给全部条目算行数） | 1.8–3.4 s | 1.8–2.0 s |
| 首帧合计（`perf_tui_resume` p50，同机 load ~300） | 3 574 ms | **2 325 ms** |

1k：70 → 56 ms；10k：556 → 379 ms。目标 50k ≤ 4 s 达成。`RADIAN_TRACE_PHASES` 的打点已收成
`agent_core::trace`（一个 `OnceLock` 只读一次环境变量），关时每次调用只多一个分支。

**不依赖墙钟的测试与变异**：`radian-session` 的 `testing` 特性给 `Session::open` 计数，
`crates/radian-host/tests/session.rs` 的 `a_resume_parses_the_journal_once` 断言 resume 恰好解析
一次。变异（`open_journal` 传 `None`，或驱动 `recovery_parse` 传 `None`）→ 计数变 2，测试失败。

### 块布局（blocks2，ADR 0028 规则 1/2/5）

容器自己拥有块间空行（规则 1）、两格标记列（规则 2）、折叠占位符的 `▸` 移到标记列（规则 5）。
热路径（50k 条目的稳态帧）多出的是：每帧对相邻条目求一次 `block_gap`（两次 kind 比较，
call/result 对再比一次 call id），以及窗口循环里的空行行。首版给 `gaps()` 每帧分配一个
`Vec<usize>`（50k push），已改为不分配的求和 + 按下标取相邻条目。

同机对比（release，`perf_render_frame_time` / `perf_render_resize`，共享机器 load 55–150）：

| 指标（50k 条目 @120×40） | main `257c33d` | blocks2 |
|---|---|---|
| 稳态帧 p50 | 0.74–1.34 ms | 0.80–1.14 ms |
| 全局重排后首帧（@80/@120 p50） | 60–64 ms | 50–66 ms |

R7 的门槛（稳态 < 1 ms、重排后首帧 < 100 ms）两边都在同一带内：稳态在共享机器 load 高时
会越过 1 ms（两边的 p50 都如此，不是本片引入），首帧 < 100 ms 两边都稳。结论：不退化。

### 合并工具卡片（blocks3，ADR 0028）

一次调用一张卡片：调用与结果仍两个条目，渲染层合并成带框的块（顶/底边框、`├── 标签 ──┤`
分隔线、`⟦…⟧` 元信息行）。热路径的变化：`line_count` 的 `ToolCall` 臂只做算术（不分配），
`ToolResult` 臂多算一次「有没有真正的输出段」和「有没有元信息」——后者先写成 `card_meta()`
（会构造字符串）再改成不分配的 `has_meta()`（50k 条目重排时会逐条跑）。稳态帧只多画可见的
几行边框；计数仍走缓存。

同机对比（release，`perf_render_frame_time` / `perf_render_resize`，共享机器 load 17–80，
同一 target 目录来回重编，故数字噪声大）：

| 指标（50k 条目 @120×40） | main / blocks2 | blocks3 |
|---|---|---|
| 稳态帧 p50 | 0.74–3.88 ms（同一台机不同 load 差 5×） | 1.12–2.00 ms |
| 全局重排后首帧 p50 | 56–66 ms | 66–69 ms |
| 全局重排后首帧 p95 | 59–73 ms | 74–86 ms |

结论：与 blocks2/main 同带，无退化；R7 的首帧 < 100 ms 稳；稳态 1 ms 仍是共享机器 load 高时
会越过（两边都如此，非本片引入）。

### 合并 main 后的复核（blocks3，main `29bdc70`）

合并 main 后一度量到 50k 重排首帧 185–473 ms，但那是共享机器的负载尖峰：同一台机上
`origin/main` 单独也量到 448 ms 的同一阶段。改为「同一 target 目录来回重编、取多轮最小值」
后（`RADIAN_TRACE_PHASES=1` 的 `line-counts 50000 entries`，即重排后那次冷计数）：

| 50k 冷计数（取多轮最小值） | origin/main | blocks3 |
|---|---|---|
| `line-counts` | 79 ms | 45–67 ms |

无退化。卡片的额外成本是每个工具条目一次「有没有真正的输出段」和「有没有元信息」的判断；
前者已从逐行查文本改成整段一次 trim + 扫一遍 plan（`9ad3b73`）。R7 的首帧 < 100 ms 在机器
安静时两边都满足。

## radian-tui 的 build script：把它从冷构建里去掉（build-script 片，2026-10-15）

**问题**：冷 `cargo test --workspace --no-run` 里最慢的单个单元是 `radian-tui` 的 build script
（`cargo --timings` 的 `build-script (run)`）：它把 syntect 的默认语法集与 `third_party/syntaxes`
里 vendor 的 TypeScript/TSX/TOML 合并、落成 `.packdump`。本片先量清楚它花在哪里，再比较三种做法，
采用测得最好的那个；不改高亮的行为（见下面的冻结测试）。

**方法**（同一台机器；这台机器同时跑着别的 agent 的编译，负载逐条标注，绝对值噪声大）：

- **单独跑那个 build script**：`target/<profile>/build/radian-tui-*/build-script-build`，带
  `CARGO_MANIFEST_DIR` 与 `OUT_DIR` 直接执行（3 次），阶段计时用 `Instant` 打在自己打的
  `RADIAN_BUILD_PHASE` 行上（临时仪表，未进提交）。
- **冷构建**：空 target 目录 + `cargo test --workspace --no-run --timings`，读 `--timings` 的
  HTML 报告拿每个 unit 的时间线与 `radian-tui build-script (run)`。
- **增量**：同一 target 目录里交替换 `build.rs`（两种状态只差这一个文件）再 `cargo build -p radian-tui`，
  这样两次之间的差就是 build script 的真实代价，不受负载漂移影响。
- 计时用 `time` / `Instant`，不用 sleep。

### 1. 时间花在哪（单独运行，3 次，load 78–90）

| 阶段 | 第 1 次 | 第 2 次 | 第 3 次 |
|---|---|---|---|
| `SyntaxSet::load_defaults_newlines()`（反序列化默认集） | 0.30 s | 0.21 s | 0.25 s |
| parse TypeScript（200 KB YAML） | 5.57 s | 5.10 s | 5.94 s |
| parse TypeScriptReact | 5.03 s | 5.52 s | 4.92 s |
| parse TOML | 0.04 s | 0.05 s | 0.04 s |
| `SyntaxSetBuilder::build()`（重新解析整集的上下文引用） | 0.76 s | 0.75 s | 0.83 s |
| dump 写盘（431081 B） | 0.01 s | 0.02 s | 0.01 s |
| **合计** | **11.72 s** | **11.65 s** | **12.00 s** |

`sample(1)` 采到的叶子函数全在 `regex_automata`（Thompson NFA 编译 + `util::determinize`）：syntect 的
YAML loader 对每条 pattern 调 `Regex::try_compile` 做**校验**，而 `syntect::parsing::Regex` 是懒编译的
（dump 里存的是 pattern 字符串，运行时才编译），所以这 10.5 s 编译出来的正则**用完就丢**。这是本片
的真正靶子：它不做任何有用的事，却要求每次冷构建都做一遍。

**它确实在关键路径上**：冷构建（load 118）的时间线里，build script 在 56.5 s 开始、118.8 s 结束，
而 radian-tui 的 lib/两个 bin/所有测试单元都要等它结束才能编译——整条构建最后完成的 4 个单元全是
radian-tui 的（总长 166.7 s）。同期 `cargo --timings` 报的 run 是 **62.4 s**（load 118）/ **38.1 s**
（load ~100）：那是**被抢占的墙钟**，它的 CPU 只有 ~12 s（单独跑就是 11.7 s），剩下的时间是在等核。

### 2. 三种做法的测量

| 做法 | build script run | 冷 wall（load） | user CPU | 采用？ |
|---|---|---|---|---|
| main（每次构建都生成） | 38.1 s | 89.7 s（54→108） | 7m09 | — |
| (a1) `[profile.dev.build-override] opt-level = 3` | 3.4 s | **120.3 s**（41→118） | **10m30** | 否：proc-macro 在内的所有 build 依赖都变成优化编译 |
| (a2) 只给该 build script 用到的 6 个 crate 设 `opt-level = 3`（`regex-automata`、`regex-syntax`、`regex`、`fancy-regex`、`syntect`、`yaml-rust`；实测包级 override 对 build-dependency 的**副本**也生效） | 6.5 s | 92.5 s（62→124） | 7m51 | 否：省下的 run 被这些 crate 的优化编译吃回去 |
| **(b) 预生成并提交 dump，build script 只校验 + 拷贝** | **0.2 s** | 88.5 s（75→160） | **6m46** | **是** |

(a2) 的 6 个 crate 及其副本优化编译的代价可以从 unit 表读出来：`regex-automata` 两份（8.5 + 6.4 s）、
`regex-syntax` 5.2 s、`syntect` 两份（2.5 + 3.2 s）、`fancy-regex` 三份……冷构建的总 CPU 反而涨了
约 20 s，所以 (a1)/(a2) 都放弃。

**冷 wall 在这台机器上量不出 (b) 的收益**（诚实记录）：成对测了 5 组（A 先、B 后，负载一路上升）
84.0/110.4、102.4/105.8、110.2/117.0、180.1/200.9、**166.9/167.6**（最后一组负载 ~110–120 基本同档，
user 503 s → 489 s）。构建的墙钟由「我们拿到多少核 × 总 CPU 工作量」决定，一个 12 s CPU 的单元被
删掉只值 ~2.8% 的 CPU，落在负载噪声里；而它在**单独**跑、或在 warm 目录里只换 `build.rs` 时看得清清楚楚：

| 只改 `build.rs` 后的增量重建（同一 target，交替，两次负载 91 / 78） | main | 本片 |
|---|---|---|
| wall / user CPU | 18.0 s / 14.9 s | **3.1 s / 3.2 s** |

差的正是那 11.7 s。改一行 radian-tui 源码的增量两边一样（2.4 s vs 2.2–2.7 s，两者都不重跑 build script）。

### 3. 采用 (b) 的形态

- 合并后的集合**提交**为 `crates/radian-tui/assets/syntaxes.packdump`（431081 B，
  md5 `34b85cf9241b1ceb6672355035c27c40`）。它是输入的纯函数：同一个未优化、一个 `opt-level = 3` 的
  build script 产出**逐字节相同**的 dump，所以「用 digest 判断是否需要重新生成」是可靠的。
- `crates/radian-tui/assets/syntaxes.packdump.digest` 是三个语法文件（含 recipe 字符串）的 FNV-1a 64。
  build script 先算 digest：不一致就**构建失败**并给出重新生成的命令——旧的 dump 不可能被静默用掉。
- build script 还会用**同一个** syntect 把 dump 反序列化一次：syntect 升级导致 dump 格式变化时，
  失败发生在构建期，而不是第一段代码高亮时。
- 重新生成是显式的：`RADIAN_REGENERATE_SYNTAXES=1 cargo build -p radian-tui`，它重写两个文件并打
  `cargo:warning=regenerated …`，提交即可。这条路径保留在 `build.rs` 里，所以 `[build-dependencies]`
  的 syntect 保持不变（冷构建的**编译**工作量因此与 main 完全相同）；把生成代码挪出构建图（可选
  build-dependency + feature，或单独一个 crate）能再省下那 10 个 unit 约 23 s 的 unit 墙钟（~12 s CPU），
  但要多一套机制，本片没有取。
- 风险与体积：仓库 +431 KB 二进制 +25 B digest（这是仓库里第一个二进制 blob；运行时二进制体积不变，
  dump 本来就嵌在可执行文件里）。风险是 dump 与语法文件不同步——由构建期 digest 校验 + 「改语法必须
  重新生成」的命令覆盖；**行为**是否有变，由 `crates/radian-tui/tests/highlight.rs` 新加的冻结测试
  （TS/TSX/TOML 固定片段的每一段文字与角色）覆盖，变异检查（把 TOML 注释的 scope 改成
  `constant.numeric.toml` 再重新生成）会让它失败。

### 局限 / 未测

- 绝对值只在这台 M4 / macOS 27 / APFS 上、且这半天负载 36–231（别的 agent 在编译）的时段成立；
  冷 wall 的 A/B 差落在这个噪声里，本片只声明「run 的 CPU 与其他所有选项」的对比。
- 没有在空载机器上重测冷构建；按上面的模型（墙钟 = 总 CPU / 拿到的核数），空载时这 11.7 s 会
  1:1 少在墙钟上，但**未实测**。
- `aws-lc-sys` 的 build script（C 编译，同一台机器 19.4–46.7 s，本片时段里常是下一个最慢的单元）
  不属本片范围，未动。
## Resume 首帧：`push_message` 的第二次 O(n²) 与 Markdown 只数不建（resume-perf2）

**方法**：一段一次性 Python pty 探针（`pty.fork` + `TIOCSWINSZ`，24×100，与
`perf_render.rs` 的 `libc::openpty` harness 同判据：读到状态栏 `context budget off`），以
`RADIAN_TRACE_PHASES=1` 跑 release 的 `radian --resume <id>`，把子进程 stderr 的 `[trace]`
段表读出来。为什么不用 `perf_render.rs` 的 `dbg_resume_trace`：它的 `wait_for` 上限是 180 s，
在负载 500+ 的几分钟里连 1k 都等不到首帧而超时；这段探针把上限放到 1200 s 只为穿过负载尖峰，
判据与取样方式不变。会话由一段同格式的生成器写出，与共享的
`crates/radian-session/tests/common/long_session.rs` 同一混合（1k/10k/50k → 1.6/16.3/81.8 MB，
退出时校验解析）。两个脚本都在 `.radian/`（gitignored），不入库。

**负载**：本机 `uptime` 1 分钟负载 **~97–120**（基线各节 0.4–570；先前的 pty 测量在负载
500+ 时甚至等不到首帧）。每档 3 次。

**一个测量前提**：pty 探针里 `enter_terminal` 会调 `supports_keyboard_enhancement`，终端的
pty 既不回 kitty flags 也不回 device-attributes，于是付满 **2 s**（`terminal.rs` 写明了这条；
真实终端只付毫秒）。所以 `terminal-ready` 里含这固定的 ~2.0 s——下表用
`first-frame-done − 2.0 s` 作为「真实终端」的估计，并同时给出原始值。

50k（66 672 条目），三次：

| 段（进程起） | 本片前 run1/2/3 | 本片后 run1/2/3 |
|---|---|---|
| `open/parse-once` | 481 / 284 / 218 | 247 / 186 / 466 |
| `open-session-done` | 1342 / 868 / 944 | 928 / 607 / 1823 |
| `terminal-ready`（含 2 s pty 查询） | 3351 / 2888 / 2986 | 2955 / 2612 / 3859 |
| `history-read` | 0（= terminal-ready，见下） | 0 |
| **`entries-built` 增量** | **1473 / 872 / 3050** | **22 / 32 / 24** |
| **`line-counts`（66 672 条目）** | **1373 / 468 / 1644** | **65 / 87 / 74** |
| `first-frame-done` | 6204 / 4238 / 7693 | 3061 / 2748 / 3963 |
| 减掉 2 s pty 查询 | ~4200 / 2200 / 5700 | **~1050 / 750 / 1950** |
| pty 首帧 `FIRST_FRAME_MS` | 6215 / 4246 / 7703 | 3075 / 2758 / 3973 |

1k / 10k 的 `first-frame-done`：本片前 2226 / 2415，本片后 2146–2185 / 2225–2532
（小档位里 2 s 查询占绝对多数，差别看不清）。

**两处原因**

1. **`push_message` 还有第二次 O(n²)（本片修）**。每个工具结果先用一次**从后往前**的扫描
   按 call id 找到它的调用（这一步本来就对），然后又用 `entries.iter().position()` **从头**
   扫一遍拿插入下标——resume 时调用就在末尾，这次前扫要走过几乎整个 transcript。resume-perf A
   只修掉了每条消息的 `retain` 全扫，没看到这一条。修法：只扫一次，复用从后扫已经拿到的下标。
   50k 的 `entries-built` 增量从 ~0.87–3.05 s 降到 ~22–32 ms。不依赖墙钟的测试：
   `app::tests::tool_result_pairing_does_not_walk_the_transcript`（`TOOL_PAIR_SCANS`
   统计配对走过的条目数；变异回前扫 → 400 个结果走 240 200 条，测试失败）。

2. **首帧给每个条目算行数时**不再把 Markdown 段落**排出来**（本片修）。此前
   `line_count_cached` 对每个 Assistant 条目调 `fill_md_cache`，即 `md_lines`——解析
   pulldown、建 `BodyLine`、逐字符建 cells；50k 的 `line-counts` 因此 ~0.47–1.64 s。现在量测走
   `blocks::md_line_count`：与 `md_lines` **逐臂镜像**地只数行（不建行），把每个段落的行数写进
   `Entry.md_count_cache`（键 `(width, text_len)`，不含折叠，所以 Ctrl-T 切换仍不重解析），而
   **行缓存改由真正要排版三路径（可见窗口、选择、搜索）懒填充**。`wrap_line_count` 另给 ASCII
   快路径（同样的宽度，不走 Unicode 表）。50k 的 `line-counts` 降到 65–87 ms。
   不依赖墙钟的测试：`blocks::tests::the_measure_pass_never_lays_out_markdown`（`md_line_builds`
   计数；变异成 `md_lines(..).len()` → 失败）、`a_repeat_frame_reuses_the_markdown_line_cache`
   （懒填充；变异成不填 → 失败）、`md_line_count_matches_the_layout`（20 个 Markdown 源 × 7 个
   宽度，与 `md_lines(..).len()` 逐一对齐；变异成不补结尾换行 → 失败），以及
   `virtual::every_window_matches_the_full_wrap`（语料加了两种 Assistant Markdown；
   `wrap_line_count` 的 ASCII 宽度变异在这里被抓住）。

**无退化**：R7 的两个门槛不变——量测仍然只数不建，`md_count_cache` 让折叠切换不重解析（
`a_fold_toggle_reuses_the_markdown_counts`）；稳态帧、改宽首帧走的是 `layout`/`md_lines_cached`
的既有路径，只是行缓存由懒填充提供，重复帧命中（`a_repeat_frame_reuses_the_markdown_line_cache`）。
带空行边的条目（首行/末行是空白段，即文本以换行开头或结尾）仍走 `line_count` 原有的
`layout` 回退——边缘空白由排版剥掉，那是规则唯一的所在；resume 语料没有这种条目。

**第 2 步（第二次完整读）在本 base 上已经是 resume-perf B 完成的**：`history-read` 在
`terminal-ready` 之后再花 0 ms（`main.rs` 取 `host_session.history`，不再 `Session::open`）。
本片补了任务要求的对照测试：
`radian-host/tests/session.rs::a_resumed_history_matches_the_disk_fold_through_compaction_and_replacement`
——一份含「已提交压缩替换 + 文件末尾未提交替换」的 fixture，直接断言 host 的初始
`history` 与 `Session::open(path).history()` 逐条相等（未提交段被丢弃、已提交段保留）。

**合并 origin/main（`39c67ad`）后**同法复测（同负载区间）：50k `line-counts` 52 / 97 / 52 ms、
`entries-built` 增量 27 / 25 / 17 ms、`first-frame-done` 2731 / 3249 / 2990 ms，与本表一致；全套
Verify（含 1.88）已在该合并树上重跑。

**本片没做**：`open_session`（~0.6–1.8 s，parse-once + replay + recover 建 records）没有动；
`terminal-ready` 里的 2 s pty 查询是探针环境的假象，真实终端不付；100k 条、真实 provider 首
token 未测。


## 文件工具 `spawn_blocking`（file-tools-parallel，本项改前）

方法：`crates/radian-coding/tests/perf_file_tools.rs`（全部 `#[ignore]`），debug 构建。
单次 grep 与同一任务里 `tokio::join!` 的三个 grep（Core 批量的形状：一个任务轮询三个
`session.execute` future），pattern 都是全树扫完也命中不了的 needle（输出只有一个
`[no matches]`，墙钟 = walk + 读文件）。本仓库以相对 `.` 为 workspace 搜；
大树是 `~/.cargo/registry/src` 下最大的展开树（`index.crates.io-1949cf8c6b5b557f`，
约 601 MB），以绝对路径搜（`CodingTools::new` 从环境解析只读根）。各 1 次 warmup，
本仓库 5 次、registry 3 次取中位。

```sh
cargo test -p radian-coding --test perf_file_tools -- --ignored --nocapture --test-threads=1
```

**负载**：`uptime` 1 分钟负载约 24（同一台机器上还有别的编译）。

| 树 | 单次 grep 中位 | 同一批三个 grep 中位 | 比值 |
|---|---|---|---|
| 本仓库（worktree，`.radian-worktrees` 被 walk 剪掉） | 162.6 ms | 504.8 ms | **3.10×** |
| registry 最大展开树（601 MB） | 1.89 s | 5.83 s | **3.08×** |

**结论：串行**。三个并发 grep 的墙钟 ≈ 单次的三倍：`run` 里的同步 walk + 逐行读从来不
让出，一个任务里的批量只能一个一个跑。`shell` 描述里的"run in parallel"到此未经证实；
本项把扫描搬进 `spawn_blocking` 后重测（见下节）。

## 文件工具 `spawn_blocking`（file-tools-parallel，本项改后）

同一命令、同一台机器（`uptime` 负载约 30）。改动：`grep`/`glob` 的 `run` 只做参数
解析与限额计算，fd walk + 逐文件读 + 渲染搬进 `tokio::task::spawn_blocking`（Core
不改；取消时丢掉 future，阻塞侧读完即弃，只读所以无害）。

| 树 | 单次 grep 中位 | 同一批三个 grep 中位 | 比值（改前） |
|---|---|---|---|
| 本仓库 | 174.8 ms | 200.9 ms | **1.15×**（改前 3.10×） |
| registry 最大展开树 | 1.77 s | 1.57 s | **0.89×**（改前 3.08×） |

**结论：并行**。三个并发 grep 的墙钟 ≈ 单次，不再是三倍。`shell` 描述里的"run in
parallel"至此证实，无需改字。`glob` 走同一条路（同一实现形状）；重叠的确定性证明
见 `crates/radian-coding/tests/file_tools_parallel.rs`（`resolve_hook` 里的 `Barrier`，
不依赖墙钟），取消见同一文件的 `a_cancelled_batch_returns_without_the_scan_result`。

## 文件工具 `spawn_blocking`（read，本项改前改后）

方法：`crates/radian-coding/tests/perf_file_tools.rs::perf_read_single_vs_batch_big_file`
（`#[ignore]`，debug 构建）。workspace 里一个 100 万行的大文件（`big.txt`，约 46 MB）：
单次 `read`（深 `offset` + `limit: 50`，墙钟主要是跳到 `offset` 的字节扫描）与同一任务
里 `tokio::join!` 的三个 `read`（三个不同的深 `offset`，Core 批量的形状）。改前数字是把
`read.rs` 临时 revert 到 `spawn_blocking` 之前跑出来的，跑完即恢复（`git diff` 确认）。
各 1 次 warmup，5 次取中位。

```sh
cargo test -p radian-coding --test perf_file_tools -- --ignored --nocapture perf_read_single_vs_batch_big_file
```

**负载**：`uptime` 1 分钟负载约 18（同一台机器上还有别的编译）。

| | 单次 read 中位 | 同一批三个 read 中位 | 比值 |
|---|---|---|---|
| 改前 | 186.4 ms | 516.0 ms | **2.77×** |
| 改后 | 183.5 ms | 205.9 ms | **1.12×** |

**结论：并行**。改前三个并发 read 的墙钟 ≈ 单次的 2.8 倍（串行），改后 ≈ 单次
（1.12×）。`read`（含目录列举，同一条路）至此与 `grep`/`glob` 一致；`shell` 描述里的
"run in parallel"对 `read`、`grep`、`glob` 三者都成立，无需改字。重叠的确定性证明见
`crates/radian-coding/tests/file_tools_parallel.rs` 的 `two_reads_in_one_batch_overlap`
与 `two_directory_reads_in_one_batch_overlap`（`resolve_hook` 会合，不依赖墙钟）。
