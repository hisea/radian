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
| `agent --resume` | 1 000 条 | 14.6 MB |
| `agent --resume` | 10 000 条 | 34.6 MB |
| `radian-tui` | 全新 | 11.5 MB |
| `radian-tui --resume` | 1 000 条 | 14.1 MB |
| `radian-tui --resume` | 10 000 条 | 38.1 MB |

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

### `agent --resume <id> -p x`：启动到第一个请求

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

- **`radian-tui --resume` 到第一帧、以及首帧后 `Ctrl-Home` 滚动到顶的耗时**：未测（本片暂停
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

### `radian-tui --resume` 到第一帧，与首帧后 `Ctrl-Home`

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
