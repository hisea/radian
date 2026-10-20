# 工具来源在注册时打标签，替代 `contains("__")` 推断 MCP

- 状态：idea（与 `mcp-servers-json.md` 一起做）
- 涉及 ADR：0012（`ToolMatcher::Mcp`）、0043 §4
- 来源：外部审核 2026-10-20 P2-4

## 背景

`ToolMatcher::Mcp` 今天是 `tool.contains("__")`，README 自己标为近似。核对：内建工具名没有一个含 `__`，MCP 别名由 `agent-extensions::alias` 统一生成，所以今天不会误判；但一个 skill 或将来的工具若名字带 `__` 就会被当成 MCP。

## 待决

- 注册时给每个工具一个来源标签（内建 / MCP server 名 / skill），`ToolMatcher::Mcp` 改查标签。标签要随工具表传到权限层，`agent-core` 的工具注册接口可能要加一个字段，这是 API 变更，要说明理由。
- 是否同时让面板的 tools 页（`tui-inspector-panel.md`）直接显示这个标签。
