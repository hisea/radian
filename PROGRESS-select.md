# PROGRESS: pi/select-copy（transcript 鼠标拖动选择复制）

Brief: scratchpad/briefs/select-copy.md · worktree ../radian-pi-select · base ceed83d

- [ ] S1 终端层：多开 `?1002h`，同一开关同一恢复路径；不开 `?1003h`；pty 断言所有退出路径有 `?1002l`、纯移动零事件
- [ ] S2 映射核心：`wrap_char_offsets`（wrap 的字节索引原语，`wrap` 由它派生）+ `select.rs`（Anchor/locate/logical_text/highlight_rows）+ 单测（折行/宽字符/空行/逻辑文本）
- [ ] S3 事件与状态：events.rs 左键 Down/Drag/Up 映射（Shift 丢弃→原生选择）；App 选择状态、按下即清除、单击清除、Modal/picker 外不开始、`tui.mouse=false` 全部不存在；拖动边缘自动滚动（仅拖动期间 recv_timeout，空闲零重绘不破坏）
- [ ] S4 剪贴板：`clip.rs`（OSC 52 base64、1 MiB 上限、ClipboardMode）+ `tui.clipboard = osc52|pbcopy|off` 配置 + loop 执行 + 诚实提示（"copied N chars (via OSC 52)"）
- [ ] S5 渲染：主题 `selection` 角色（pi `selectedBg`；mono=REVERSED）+ draw 高亮选区
- [ ] S6 pty：SGR 按下/拖动/松开 → `ESC]52;c;<base64>`；退出路径 `?1002l`
- [ ] S7 双击选词 / 三击选整条（时间不够则写进报告）
- [ ] S8 README（Shift+拖动后备说明）、MUTATIONS.md 变异、Verify 全套 + pty×3

提交前删除本文件。
