# 面板：`jobs | tools | mcp | hooks` 共用一个带标签页的覆盖层

- 状态：decided＼�mcp 页等 `mcp-servers-json.md` 落地）
- 涉及 ADR：0023 U4（层栈，新增一个 Overlay）、0035（`panel.rs` 共享覆盖层框）、0037 决定 4（`/jobs` 从通知改为面板，文本形式保留）、0043 §4（tools 页展示实际请求里的工具集）、0045（hooks）、0027
- 来源：`docs/research/2026-10-20-peer-slash-commands.md` §3 第二档；「最好有一个统一 TUI，和 jobs 共享」

## 背景

`/jobs` 今天是一串通知；`/tools`、`/mcp`、`/hooks` 还没有。四个都是「列一张表、看一个详情、偶尔杀一个」，分开做四套通知不如一个面板。TUI 已有零件：`panel.rs` 是共享覆盖层框，`picker.rs` 是已并入层栈的 Overlay 列表。

## 决定（已定）

- 一个 Overlay（0023 U4），四个标签页 `jobs | tools | mcp | hooks`；以后 `/context` 的分类表可加为第五页。
- 入口：`/jobs`、`/tools`、`/mcp`、`/hooks` 各打开对应页；`/panel` 打开上次停留的页。左右键或 Tab 换页，上下选行，Enter 看详情，Esc 关闭。
- 页内容：
  - jobs：id、状态、已运行时长、命令；详情是最近输出尾部。`k` 杀选中 job，确认一次（和 `/jobs kill` 同一路径）。
  - tools：当前请求里模型看得到的每个工具、定义大小（字节与估算 token）、来源（内建/MCP server/skill）；详情是定义全文。
  - mcp：每个 server 的来源文件（用户级或项目 `.mcp.json`）、状态（running/failed 及错误）、工具数、是否发生过 64 KiB 截断；详情是其工具列表。
  - hooks：每条 hook 的事件、匹配、命令、来源文件与层级、是否生效。
- 只读，唯一写操作是 jobs 页的 `k`。
- 文本形式保留：`/jobs kill <id>`、`/jobs kill all` 以及带参数的查询仍按通知输出，脚本与 herdr 不受影响。
- 回合进行中可打开；内容随事件刷新，不轮询（0023 U2），面板开着且空闲时零重绘。
- mcp 页在 `mcp-servers-json.md` 落地前显示「MCP 配置尚未支持列出」，其余三页不等。

## 验收

- pty：四个命令各打开对应页；`/panel` 回到上次的页；Esc 关闭回到 composer；回合进行中打开不影响流式输出。
- jobs 页 `k` 后 job 进入 killed，与 `/jobs kill <id>` 的现有测试结果一致；现有 `/jobs` 文本测试仍绿。
- tools 页列出的集合与同一时刻请求 schema 里的工具集逐名相同（0043 §4 的旧名不出现）。
- 面板开着、无事件时事件循环没有唤醒（现有「idle 零重绘」测试扩到面板层）。
