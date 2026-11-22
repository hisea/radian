# 删掉 Mermaid 渲染

- 状态：decided
- 涉及 ADR：0042、0028、0035、0008、0029
- 来源：2026-11-21 精简调查

## 背景

TUI 把闭合的 `mermaid` 代码块画成图（ADR 0042、ADR 0028 R8），渲染器是 vendored 的 `third_party/grok-mermaid`（约 195 KB，workspace 成员），默认开启（`tui.display.mermaid = true`）。pi 没有这个功能；不渲染时本来就退回显示源码。

## 决定（已定）

- 删掉 `third_party/grok-mermaid`（workspace 成员、依赖、NOTICE 里的对应条目）、TUI 里的 mermaid 渲染和测试。
- `mermaid` 代码块按普通代码块显示（语法高亮按现有规则）。
- `tui.display.mermaid` 按退役的显示类键处理：一行警告后忽略，`configuration.md` 写明 inert。主题里只为 mermaid 存在的颜色键同样退役为 inert（`themes.md`）。
- ADR 0042 改为 `Status: removed`；ADR 0028 删掉 R8，ADR 0035 删掉 mermaid 相关条款。

## 验收

含 `mermaid` 代码块的回复显示为代码块；带 `tui.display.mermaid` 的旧配置和带 mermaid 颜色键的旧主题照常加载并只警告；workspace 成员里没有 grok-mermaid。
