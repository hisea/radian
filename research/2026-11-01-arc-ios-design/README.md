# arc iPhone app：第一版视觉设计稿

> **日期**：2026-11-01
> **状态**：设计稿，idea；对应 `docs/backlog/arc-ios.md`（客户端）与 `docs/backlog/arc-serve.md`（hub 接口）。不是实现，不是手册。
> **怎么来的**：`prompt.md` 是喂给 Claude Design 的全部输入，只写了已定的东西（arc-ios.md 的形态与动作、hub 的字段、ADR 0048 的状态集、TUI 的 needs-you 排序、网站与 TUI 的视觉语言）；`arc-ios.html` 是它的输出，单文件，自带样式，浏览器直接打开。

## 里面有什么

七个界面各一组（浅色加深色）：Needs you、Plan、Task（三张：gate 结论、findings 与 diff、报告与单文件 diff）、Merge 与 Reject 单、Repositories、Pairing、通知落地；四个二次确认框（merge、reject、cancel、auto 调到 merge）；六种状态（hub 不可达、重连刷新、列表重读、hub 拒绝 401、无事可做、plan 被 max_auto_rework 暂停、hub 拒绝动作）；一页组件表（每个 `state.phase` 的字形与 SF Symbol 名、角色色值、verdict 行、finding 行、diff 行）。

## 审稿结论（2026-11-01）

对的、要保留的：needs-you 的排序与分组；findings 按级别分组、`pre-existing` 标签、每条带触发条件；diff 行的增删条；"mutation stale · Branch moved. This verdict no longer applies." 把 verdict 绑 sha 画了出来；hub 不可达时 "actions are off until it answers"；拒绝单的 "An empty note is not sent"；plan 暂停时一句话说明原因与出路；两档紫色、等宽标签、细线不用卡片。

要改的，三处与 arc 事实不符：

1. integration 在合并前就显示 pass（任务页与合并单）。按 ADR 0048，integration 在 merge 之后才跑（`awaiting_human → integrating → ready_to_merge → merged`）。合并前应是 "runs on merge, against main <sha>"，合并单上 human 与 integration 都是 "this action"。
2. mutation 阶段没有分数。"score 0.84, threshold 0.80" 是编的；它只有过与不过，失败时点名存活的变异。
3. 样例数据是 TypeScript 文件；radian 是 Rust 仓库，换成 `.rs` 路径与 cargo 风格的命令。

小的：hub 端口样例 7878，已定的缺省是 7341；"human · no verdict" 一行与底部按钮重复；`reverify`、`priority` 所在的 "⋯" 菜单没画。

## 没定的

arc-ios.md 的待决不变：代码放哪个仓库、分发方式、最低 iOS 版本（稿子按 iOS 17 画）、通知深链能否直接打开 app（稿子按 `radian://hub/repos/<name>/items/<task>` 画）。
