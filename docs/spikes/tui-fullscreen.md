# Phase 0b 可行性验证：TUI 全屏（ratatui + crossterm）

- 结论：**通过**（6 项标准全部满足，阈值未放宽）
- 环境：macOS 27.0.1（arm64）；rustc 1.99.0
- 依赖（版本经 `cargo info` 查证为 crates.io 已发布）：`ratatui 0.30.2`（MSRV 1.88，与仓库 MSRV 一致）、`crossterm 0.29.0`（经 ratatui 间接使用）、`portable-pty 0.9.0`、`vt100 0.16.2`、`unicode-width 0.2.2`、`serde_json 1.0.154`
- 原型：`spikes/tui-fullscreen/`（独立 cargo 项目）；`tui-app` 是全屏原型，`tui-driver` 经 pty 驱动并产出证据
- **全屏原型从未在任何真实终端面板里运行过**：全部经 `portable-pty` 的 pty（120×40）驱动，渲染结果用 `vt100` 屏幕模型断言；纯布局成本另用 `ratatui::backend::TestBackend` 测。两类数据在下表分开标注。

## 结果总表

| # | 标准 | 结果 | 数据来源 | 证据 |
|---|---|---|---|---|
| 1 | 流式 200 token/s，120×40 下 p95 单帧 < 16 ms | ✅ p95=1.89 ms（首次）/6.03 ms（复测），max≈42 ms | **真实 pty** | `data/stream-pty.json`（10 s，tokens=2001≈200/s，frames=1203–1413） |
| 1' | （同上，纯布局成本参照） | ✅ p95=1.59 ms | **TestBackend** | `data/stream-test.json`（10 s，tokens=2001） |
| 2 | 多行 bracketed paste 5000 字符不丢不卡 | ✅ 5000 字节全到（校验和一致），13 ms 后状态行即显示 | 真实 pty | `data/paste.json`、`data/paste-summary.txt`；paste 期间最大帧 0.86 ms |
| 3 | CJK 与 emoji 宽度正确 | ✅ 7 个样例（中文/日文/emoji/全角/谚文/半角片假名/组合符）与 unicode-width 预期全部一致 | 真实 pty + vt100 模型 | `data/width-samples.txt` |
| 4 | resize 后无残影 | ✅ 120×40→80×24→120×40 两轮，边框四角与内部填充逐格核对 | 真实 pty + vt100 模型 | `case_resize`（`src/lib.rs`），测试 `resize_leaves_no_residue` |
| 5 | 空闲 CPU < 1% | ✅ 30 s、每秒采样 `ps -o %cpu=`，30 个样本全为 0.00% | 真实 pty | `data/idle-cpu.csv`、`data/idle.json` |
| 6 | 退出后终端状态恢复（含 panic 与 Ctrl-C） | ✅ 三条路径（q / Ctrl-C / panic）均复原 | 真实 pty | 字节日志核对 + 退出后同 pty 内 `stty -a` 显示 cooked（见下） |

## "一帧"与测量方法

- 一帧 = 一次因**新内容到达**而执行的 `Terminal::draw` + backend flush（含向 pty 写出的全部字节）。空闲重画不计帧。app 用 `Instant` 包住 draw+flush 计时，逐帧记录在 stats JSON 的 `frame_ms` 数组里，p50/p95/max 由驱动侧与测试共同核对（p95 取 nearest-rank）。
- token 流：独立线程按**绝对节拍**（`start + i×5ms`）发射，实测 10 s 恰好 2001 个 token（相对 sleep 会漂移到 ~165/s，已修）。事件循环每 5 ms 预算内合并到达的 token 后画一帧。
- pty 与 TestBackend 数据严格分开：pty 数包含终端写入成本；TestBackend 数只反映布局+差分成本。**真实终端模拟器（Terminal.app/iTerm）的渲染耗时未测量**（见"未验证"）。

## paste 细节

- 驱动侧发送 `\x1b[200~` + 5000 字节多行文本 + `\x1b[201~`；app 的 `Event::Paste` 按字节累加并算 wrapping 校验和，状态行显示 `paste:<bytes>:<sum>:<events>`。驱动等待该标记出现（13 ms），退出后核对 stats：`paste_bytes=5000`、`paste_sum=538382`（与发送侧一致）、`paste_events=1`（多行未拆事件、未丢字符）。

## 宽度细目（`data/width-samples.txt`）

| 样例 | unicode-width 预期 | vt100 实占 | 判定 |
|---|---|---|---|
| 中文abc | 7 | 7 | 一致 |
| 日本語の幅 | 10 | 10 | 一致 |
| 🎉🚀emoji | 9 | 9 | 一致 |
| ＦＵＬＬｗｉｄｔｈ | 18 | 18 | 一致 |
| 한국어 | 6 | 6 | 一致 |
| ｱｲｳ半角 | 7 | 7 | 一致 |
| e+组合符+组合 | 5 | 5 | 一致 |

测量方式：内容渲染在两个 `|` 之间，实际占宽 = 两个 `|` 的 vt100 列位置差 − 1；宽字符在 vt100 模型里占两个 cell，该差值即双格证据。ratatui（unicode-width）与 vt100 模型一致；**真实终端的字形宽度行为未验证**。

## resize 细节

- 驱动 `master.resize()` 后同步把 vt100 模型 `set_size`，等 app 状态行出现新尺寸再核对：四角 `┌┐└┘`、内部填 `X`、屏幕尺寸与 pty 一致。缩小→放大两轮都过，无旧尺寸残留。

## 终端状态恢复细目（3 条路径）

app 用 `TtyGuard`（Drop）+ panic hook 双保险。验证依据（字节日志 + 同 pty 后续输出）：

- 退出序列存在且成对：`\x1b[?2004l`（bracketed paste 关）、`\x1b[?1049l`（退出备用屏）、`\x1b[?25h`（光标恢复）；
- app 退出后**同一 pty 里**的 shell 执行 `stty -a`，输出含 `icanon` 且不含 `-icanon`（raw mode 已复原）——这条覆盖了 `disable_raw_mode` 真正生效，而不是只看转义序列；
- 三条路径：`q`（退出码 0）、Ctrl-C（raw mode 下作为按键处理，退出码 0）、`p` 触发 panic（退出码 101，hook 先复原再打印 panic）。

## 复现

```sh
cd <home>/Code/radian-pi-spikes/spikes/tui-fullscreen
cargo build --bins          # 重要：tui-driver 起的是 tui-app 二进制，改代码后必须两个 bin 一起重建
for c in stream-pty stream-test paste width resize restore-normal restore-ctrlc restore-panic idle-cpu; do
  cargo run --bin tui-driver -- $c
done
cargo test                  # 9 个测试覆盖同一路径（时长缩短）
```

孤儿进程检查：`ps -axo pid,comm | grep tui-app`（无输出）。

## 未验证的部分

- **真实终端模拟器**（Terminal.app、iTerm2、Alacritty 等）下的行为：帧耗时、字形宽度、resize 残影。本 spike 的"真实 pty"指内核 pty 设备 + vt100 屏幕模型，不是任何模拟器；`crossterm` 发出的转义序列已核对，但模拟器自身的实现差异（尤其 emoji 宽度、连字）未测。
- 未测 kitty 键盘协议、鼠标事件、焦点事件。
- 未测超大 transcript（数万行）下的滚动虚拟化——Phase 5 的"高效 transcript virtualization"是单独课题。
- 空闲 CPU 用 `ps` 采样（粒度 0.1%）；更细的功耗/唤醒次数未测。
- Windows/Linux pty（portable-pty 支持跨平台，本任务只验证 macOS）。
- 16 ms 阈值对应 60 FPS；本测法只统计有新内容的帧，不含空闲周期。
- 流式期间的 p95 在两次运行间波动（1.89→6.03 ms），与当时机器上并行的构建负载相关；两次都远低于阈值，但**负载下的长尾**值得产品化时用 CI 基准再压一遍。

## 对计划的影响

- 证实：v5 计划 Phase 4 的全屏方案（ratatui + crossterm、差分渲染、bracketed paste、resize 处理、panic/Ctrl-C 恢复）在 macOS 上可行且余量充足（p95 ≪ 16 ms）。
- 证实：ratatui 0.30 MSRV 恰为 1.88，与仓库 MSRV 对齐，无版本障碍。
- 修正一条工程事实：**跨 bin 的驱动测试必须先重建全部 bin**（`cargo run --bin tui-driver` 不会重建 `tui-app`），否则测的是旧二进制。产品化时应让驱动从 `CARGO_BIN_EXE_*` 取路径（集成测试已这样做了）。
- 待定（给决策）：inline 模式（不用备用屏）是另一张 spike（0b 表格里的"TUI inline"），本任务不含；全屏通过不等于 inline 通过。

## 复核补充（owner 真实终端手测，2026-10-12）

owner 在 Terminal.app（macOS 27.0.1）里按 `--mode stream|width|resize|paste` 各跑了一遍：流式刷新、CJK/emoji 对齐、resize 重排、bracketed paste 的字节统计**均正常**，基础功能通过。本文档"未验证"一节里的"真实终端模拟器"一项就此关闭（仅限 Terminal.app / macOS 27.0.1；其他模拟器仍未验证）。

发现一处观感问题：**原型退出时没有补一个换行**，真实 shell 下 zsh 会显示它的"输出未以换行结尾"标记 `%`（提示符本身仍落在新行，所以不影响正确性）。pty 自动化看不到这个标记。Phase 4 实现时退出前补一个换行。
