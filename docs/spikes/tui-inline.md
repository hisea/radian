# Phase 0b 可行性验证：TUI inline（ratatui + crossterm）

- 结论：**不通过。inline 不进首发。**（2026-10-12 由 owner 在真实终端人工验证后改判；原先的"通过（有代价待接受）"是 pty 环境下的结论，见下节"改判"。）
  任务单的四条门槛在 pty 驱动下确实全部通过，原型也确实从不进入备用屏；但在真实终端里**拖动窗口改变宽度会让原型崩溃**，并把终端应答泄漏成 shell 的键盘输入。这不是"清屏是否可接受"的取舍问题，而是 inline viewport 的实现方式在真实终端下的硬缺陷。
- 环境：macOS 27.0.1（arm64）；rustc 1.99.0。
- 依赖（均先以 `cargo info` 确认在 crates.io 发布）：`ratatui 0.30.2`、`portable-pty 0.9.0`、`vt100 0.16.2`、`serde_json 1.0.154`。`ratatui 0.30.2` 的 MSRV 是 1.88，符合仓库要求。
- 原型：`spikes/tui-inline/` 是带空 `[workspace]` 的独立 cargo 项目；`tui-inline-app` 是原型，`tui-inline-driver` 只通过 `portable-pty` 驱动它并写证据。**本次没有在 agent 自己的终端面板直接运行 app。**


## 改判：真实终端下 resize 崩溃＼�2026-10-12）

### 观察到的现象

owner 在 Terminal.app（macOS 27.0.1）里按本文档"需要 owner 人工验证"一节的步骤执行：

- **测试一（scrollback 共存）通过**：`PRE-0001`…`PRE-0050` 与原型推上去的 `LINE-0000`…`LINE-0029` 全部留在 scrollback 里，可以向上滚动、可以用终端原生方式选择复制；向上滚动时 inline 区域随之滚出视野——这是 inline 的正确行为（它不占备用屏，滚动就是滚进真实历史）。
- **测试二（resize）失败**：原型在输出过程中把窗口从宽拖窄时 panic：

  ```
  thread 'main' panicked at src/app.rs:198:14:
  draw: Custom { kind: Other, error: Custom { kind: Other,
    error: "The cursor position could not be read within a normal duration" } }
  ```

  崩溃后 shell 提示符上出现了 `;42R`——那是迟到的光标位置应答（CPR），app 已经退出，应答落进 shell 的输入缓冲，被当成用户敲的字符。

### 根因（在依赖的源码中核实，不是原型的 `expect` 写法问题）

1. `ratatui_core::terminal::resize()` 对 `Viewport::Inline` 调用 `compute_inline_size()`（`ratatui-core-0.1.2/src/terminal/inline.rs:396`）；
2. `compute_inline_size()` 的**第一行**就是 `backend.get_cursor_position()?`——inline 必须知道自己在第几行才能定位区域；
3. crossterm 的实现（`crossterm-0.29.0/src/cursor/sys/unix.rs:17`）向终端发 DSR（`ESC[6n`）并**阻塞等待** CPR 应答，超时即返回上面那句错误；
4. 窗口拖动期间这个往返超时，`draw` 返回 `Err`，原型 panic。

**与全屏的架构性差异**：`ratatui-core` 的 `Viewport` 文档写明"全屏模式下 viewport 从 (0,0) 开始；inline 和 fixed 模式的原点可能非 0"。全屏因此**不需要**向终端查询光标位置，inline 必须查。这不是实现质量问题，是两种模式的本质区别。

### 为什么 pty 自动化测不出来

本 spike 的自动化用 `portable-pty` + `vt100` 屏幕模型。vt100 模型会**立即、确定地**回应 DSR，永不超时；真实终端在 resize 期间不保证。因此"pty 全绿"与"真实终端可用"之间存在本质落差——这一条应作为后续所有终端相关 spike 的教训：**凡依赖终端应答（DSR/CPR、键盘增强查询、设备属性）的能力，pty 自动化不足以判定可行性，必须人工在真实终端验证。**

### 把 `expect` 换成错误处理也不够

产品里当然不该 panic，但拿不到光标位置就无法正确定位 inline 区域，只能放弃本次重画或退化显示；而且无论是否 panic，超时的 DSR 应答仍可能迟到并污染 shell 的输入。要在首发支持 inline，必须绕开 ratatui 的 inline viewport、自行管理行位置且不依赖 DSR——那是另一个量级的工作，不属于首发范围。

### 对计划的影响

- 计划 v5 Phase 4 的「inline 仅在其 spike 满足正确性/可用性门槛时进入首发」据此落地为：**不进首发**。Phase 4 只做全屏。
- 全屏模式的 scrollback、选择复制、搜索、长 transcript 滚动，都必须由我们自己实现（计划 Phase 5 的 "transcript virtualization" 正是这件事），不能指望终端原生能力。
- 本原型与全部证据保留在仓库里：它回答了"inline 能否进首发"，答案是不能，重做时不必从零再验。
- **给 Phase 4 的一条小记**：全屏 spike（`docs/spikes/tui-fullscreen.md`）退出时没有补一个换行，真实 shell 下会显示 zsh 的 `%` 标记（pty 测不出，手测看到）。实现时退出前补换行。

## 实现边界

- 使用 `TerminalOptions { viewport: Viewport::Inline(height) }`，布局只以 `Frame::area()` 为根；没有假定 inline viewport 的原点是 `(0, 0)`。
- 流式行通过 `Terminal::insert_before(1, ...)` 插入 viewport 上方，故普通终端会把它们纳入其滚动历史，而不是只刷新固定的 TUI 区域。
- 不使用 `ratatui::init_with_options` / `ratatui::restore`：后者会写 `?1049l`。本原型只自行开启 raw mode 和 bracketed paste，并在 normal、Ctrl-C、panic 三条路径关闭 paste、显示光标、恢复 cooked mode；从不写 `?1049h` 或 `?1049l`，也不启用任何鼠标上报。
- `Viewport::Inline` 会发 DSR 光标位置查询（`CSI 6n`）。pty driver 按其 vt100 当前光标回送 `CSI row;col R`，模拟真实终端对该查询的回复；这不是把 vt100 当作真实终端模拟器的结论。

## 结果总表

下表所有“真实 pty”均指内核 pty + 原始字节捕获；它不是 Terminal.app、iTerm2 或其他真实终端模拟器。

| # | 门槛 | 结果 | 数据来源 | 证据 |
|---|---|---|---|---|
| 前提 | 不使用备用屏 | ✅ 所有门槛用例的原始字节流均不含 `?1049h` / `?1049l` | 真实 pty | `data/scrollback.txt`、`resize.txt`、`selection.txt`、`prompt-*.txt`；断言在 `src/lib.rs` |
| 1 | 流式输出与既有 scrollback 共存 | ✅ `PRE-` 恰 50 次、`LINE-` 恰 30 次；无备用屏、鼠标捕获或 `2J`；vt100 顶部可见最早 PRE，正常视图有 `pushed:30/30` | 真实 pty + vt100（范围见下） | `data/scrollback.txt`；测试 `streaming_coexists_with_scrollback` |
| 2 | 120×40 → 60×40 → 120×40，期间重排且无错位/残影 | ✅ 三个宽度状态标记按序出现，60 行流式输出完整；resize 后以清屏重画消除残影 | 真实 pty + vt100 状态标记 | `data/resize.txt`、`data/resize-stats.json`；测试 `resize_keeps_inline_layout_correct` |
| 3 | 选择/复制的终端原生前提 | ✅ 无备用屏、无 `?1000h` / `?1002h` / `?1003h` / `?1006h` / `?1015h` 鼠标上报；内容进入 scrollback；paste 最终关闭 | 真实 pty + vt100 | `data/selection.txt`；测试 `native_selection_preconditions_hold` |
| 4 | 退出后 shell 提示符正确 | ✅ q、Ctrl-C 均退出码 0；panic 为 101；三条路径均 cooked、paste 开关成对、光标恢复，`MARKER-$$` 在新行行首、上方空行 0、光标列 0 | 真实 pty + vt100 | `data/prompt-normal.txt`、`prompt-ctrlc.txt`、`prompt-panic.txt`；三个 `shell_prompt_position_correct_on_*` 测试 |

## 各项证据和方法边界

### 1. scrollback

driver 在同一 pty 的 shell 中依序写出 `PRE-0001` 到 `PRE-0050`，运行 app 并插入 `LINE-0000` 到 `LINE-0029`，退出后再由同一 shell 写 `EXITED:$?`、`MARKER-$$` 和 `stty -a`。

- `data/scrollback.txt` 的原始 pty 字节计数是 `PRE-` 50 次、`LINE-` 30 次，且不含备用屏、鼠标捕获或整屏清除序列。这是本项“内容真的被写给终端”的主证据。
- vt100 的 `contents()` **只显示当前 40 行视口**，不等于完整滚动历史。driver 因而把模型滚至顶端，只核对 `PRE-0001..PRE-0040`；回到正常视图核对 `pushed:30/30`，且未处于 alternate screen。不能要求同一个 40 行模型视图同时出现 `PRE-0001` 和 `PRE-0050`。
- 原始字节流说明终端接收到了哪些行，通常这些行会进入其 scrollback；但滚动历史容量、复制和回流仍由具体终端模拟器决定，未在真实模拟器自动验证。

### 2. resize

driver 调 `master.resize()`：120×40 → 60×40 → 120×40，并同步更新 vt100 模型大小；每阶段都等 app 输出 `size:<width>x<viewport-height>` 标记，最后等到 `LINE-0059` 才发送 q。因此不是用 `sleep` 碰运气。

`data/resize.txt` 记录三个标记都按序出现和 60 条流式行完整。最终 stats 的 viewport 是 `[120, 5]`（`data/resize-stats.json`），证明回到 120 列后仍是 5 行 inline 区域。

**已接受代价：**本机此实现的 resize 字节流出现一次 `CSI 2J`，随后重画。`2J` 清可见屏；没有看到 `CSI 3J`，但不应把 vt100 对旧行的处理说成真实终端的处理。任务单要求的“已滚上去的旧行按终端自身语义处理”在 Terminal.app/iTerm2/Alacritty 未验证。它是 owner 是否让 inline 进首发时必须接受或否决的风险。

### 3. 选择/复制

自动化只能验证前提，不能替终端模拟器验证鼠标选择或系统剪贴板。此原型不进入备用屏，也不启用鼠标捕获，所以终端原生选择不会被程序劫持；标准 1 已证明内容写入滚动历史路径。`data/selection.txt` 还记录 app 结束后 vt100 的 bracketed paste 状态为 false，且可见内容含 `LINE-0014`。

**需要 owner 人工验证（在自己的终端中执行，非本 spike 自动化的一部分）：**

```sh
cd <worktree>/spikes/tui-inline
cargo build --bins
for i in $(seq 1 50); do printf 'PRE-%04d\n' "$i"; done
./target/debug/tui-inline-app --lines 30 --interval-ms 80 --stats /tmp/radian-inline-manual.json
# 等 LINE 输出后按 q；上滚，鼠标拖选 PRE 与 LINE，复制并粘贴到另一处核对。
# 再重复一次：运行中把终端宽度 120→60→120，检查清屏/重画和旧行行为是否可接受。
```

### 4. 退出和恢复

每条退出路径都在同一 pty 内由 app 后的 shell 执行 `stty -a` 和 `echo MARKER-$$`。driver 检查：

- `?2004h` 与 `?2004l` 各恰好一次，且开在关之前；恢复后没有遗留 bracketed paste；
- ratatui 每帧会写 `?25l` 隐藏光标，app 的 `restore_tty()` 明确写 `?25h`；测试确认最后一个 hide 后有 show；
- `stty -a` 包含 `icanon` 且不含 `-icanon`，所以不是只依靠控制序列推测 raw mode 已恢复；
- `MARKER-$$` 是一行开头，之前连续空行不超过 2（本次都是 0），模型光标列为 0。

## 非门槛记录：高度钳制与 `insert_before` 粗测

数据都来自真实 pty，计时为每一次 `insert_before` 的 `Instant` 包围时间，包含向 pty 写入，非真实终端模拟器呈现耗时：

| 请求高度 / pty | 实际 viewport | 6 次 insert p50 / p95 / max | 证据 |
|---|---:|---:|---|
| 50 / 120×40 | 120×40 | 5.70 / 13.59 / 13.59 ms | `data/clamp-h50-term40.json` |
| 10 / 120×6 | 120×6 | 2.80 / 10.94 / 10.94 ms | `data/clamp-h10-term6.json` |
| 3 / 120×40 | 120×3 | 8.72 / 15.10 / 15.10 ms | `data/clamp-h3-term40.json` |

所以 `Viewport::Inline(height)` 在高度超过终端行数时钳制到终端高度；高度为 3 时保持 3。样本仅 6 次、受本机负载影响，上表是**单次运行**的数字（`data/` 中每个 JSON 就是那一次），只是可行性粗测，不构成性能承诺。

## 复现

```sh
cd <worktree>/spikes/tui-inline
cargo build --bins # driver 会启动 app，改动后必须同时重建两个 bin
for c in scrollback resize selection prompt-normal prompt-ctrlc prompt-panic clamp; do
  cargo run --bin tui-inline-driver -- "$c"
done
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
ps -axo pid,comm | grep 'tui-inline-app' # 本次检查无孤儿；grep 自身输出不计
```

driver 的 `data/*.txt` 和 `data/*.json` 是上表证据。测试的临时 stats 文件放在 `/tmp`，不会污染仓库；原型默认的 stats 路径也固定在本 spike 的 `data/`（`spikes/tui-inline/.gitignore` 屏蔽 `data/stats.json` 与 `data/*.tmp`），从仓库根或别处运行也不会在仓库生成未跟踪文件。

## 变异检查

在一次性 worktree `/tmp/radian-inline-mut`（基于最终原型提交 `540c215`）中，每项先修改 `src/app.rs`、执行 `cargo build --bins`，再只运行相应具名集成测试；五项都以 exit 101 失败，随后 `git restore`，最后删除 worktree。没有修改主分支 worktree。

| 变异 | 对应测试 | 被抓住的失败 |
|---|---|---|
| 在初始化额外写 `EnterAlternateScreen` | `streaming_coexists_with_scrollback` | 原始 pty 字节流含 `?1049h`，vt100 也报告 alternate screen |
| status 宽度硬编码 120、忽略 `Frame::area().width` | `resize_keeps_inline_layout_correct` | 60 列阶段的 `size:60x` 标记永不出现 |
| 初始化额外写 `EnableMouseCapture` | `native_selection_preconditions_hold` | 字节流含 `?1000h` |
| `restore_tty()` 变为空实现 | `shell_prompt_position_correct_on_normal_exit` | paste 的精确配对失败（on=1、off=0）、`stty` 仍为 `-icanon`、光标列不是 0 |
| panic 前也调用一次 `restore_tty()`（hook 会再恢复） | `shell_prompt_position_correct_on_panic` | paste 的精确配对失败（on=1、off=2） |

## 未验证

- Terminal.app、iTerm2、Alacritty 等真实终端模拟器下的 scrollback 保留/回流、`CSI 2J` 视觉影响、resize 残影、鼠标选择和系统剪贴板；尤其 resize 时旧的**可见**普通输出是否可接受，必须人工确认。
- Linux/Windows pty。
- 长 transcript 下的 scrollback 容量与虚拟化。
- 真实模拟器的渲染帧耗时和功耗；表中只测 pty 写入路径。

## 对计划的影响与待决定事项

- 技术上，v5 Phase 4 可以采用 `ratatui` inline viewport：流式 `insert_before`、无备用屏、resize 尺寸重排和三条退出恢复路径均有 pty 证据。
- **首发建议取决于 owner 对 resize `CSI 2J` 的接受度。**若不可接受“resize 时清可见屏、由终端自行保留已滚上去旧行”的体验，应让 inline 不进首发，保留已通过的全屏模式；若接受，应把上述人工终端矩阵验证列为 Phase 4 前置项，并避免产品代码无意引入 `?1049` 或鼠标捕获。
- 本 spike 不改变任何现有 crate 或公共 API。
