# 内建 skill `init` 与 `review`（`/skill:init`、`/skill:review`）

- 状态：decided＼�接受 `/skill:` 前缀形式，不加顶层别名）
- 涉及 ADR：0025（S1 内建 skill 用 `include_str!`，S3 `/skill:` 调用）、0036（AGENTS.md 的大小上限与不可信标记）、0043 §2（`explore` 只有 `read glob grep`，`general` 有父会话全部工具，一条回复里多个 `subagent` 并行）
- 来源：`docs/research/2026-10-20-peer-slash-commands.md` §3 第二档

## 背景

opencode 与 Codex 有 `/init`（生成 AGENTS.md），Codex 与 omp 有 `/review`（omp 是并行 reviewer 子代理加 P0–P3 分级）。radian 不写代码就能有：两个 `SKILL.md` 编译进二进制，走 ADR 0025 已有的发现与 `/skill:` 调用。

## 决定（已定）

- 两个内建 skill，目录 `crates/radian-host/skills/init`、`crates/radian-host/skills/review`，与 `theme-import` 同一注册方式；用户级同名 skill 照 0025 S1 覆盖它们，`[skills] disabled` 可关。
- `init`：读仓库结构与构建文件，**实际运行**构建与测试命令核实能跑，再写 AGENTS.md；已有 AGENTS.md 时只补缺、不覆盖；正文要求产出控制在 ADR 0036 的上下文文件上限内，并写明哪些命令是核实过的、哪些没有。
- `review`：参数是目标（缺省未提交改动；或分支名、提交号）。父会话用 shell 取 diff，按文件分组，**一条回复里**起多个 `general` 子会话并行审（`explore` 跑不了 git，不用它），每个子会话返回不超过 4 000 字符、按 P0–P3 分级且附文件和行的结论；父会话合并、去重、给出 ship/no-ship 一句话。只报告，不改代码。
- 不加 `/init`、`/review` 顶层命令；`/help` 不列它们，`/skills` 会列出。

## 验收

- 发现测试：两个内建 skill 出现在 `/skills`；`builtin = false` 与 `disabled = ["review"]` 分别关掉；用户级同名覆盖生效。
- `/skill:init` 与 `/skill:review` 的 pty 测试：skill 正文进入下一条用户消息（0025 S3 的现有形状）。
- 在本仓库手跑一次 `/skill:review`＼�：子会话并行启动、父会话给出合并结论；记录一次的 token 与时长到 PR 说明。
