# 过去的会话当记忆：模型可以查「上次我们怎么做的」

- 状态：decided（2026-10-31：文件形式，不加工具）
- 涉及 ADR：0021（记忆与会话的边界）、0006（会话 JSONL）、0019（只读已提交的会话事实）、0043（单会话）
- 来源：`research/2026-10-31-competitor-memory.md` §2.3（Codex）、§2.10（Amp）、§4

## 背景

Amp 让模型搜索过去的线程，包括工作区成员的线程，提到一个线程就读取并提取相关内容；Codex 的离线提炼给每个 rollout 生成 `rollout_summary` 与 `rollout_slug`，等于一份会话索引，模型通过记忆工具读它。radian 的会话全文搜索只在 TUI 的会话列表里给人用（`docs/user/sessions.md`），模型没有任何工具回答「上次我们怎么做的」；记忆文件（`memory-files.md`）存的是提炼后的结论，不是过程。

## 决定（已定）

- 文件形式，不加工具。每个 scope 为 project 的记忆目录（`~/.radian/memory/projects/<ProjectId>/`）下有一个宿主维护的只读文件 `SESSIONS.md`：每个会话一行，`- <会话 id> <开始时间 UTC> <工作区相对路径或 .> — <首条用户消息的第一行，截到 120 字符>`，新的在下。宿主在会话结束（进程退出或 `/exit`）时追加一行；只追加已提交的会话事实（ADR 0019）；当前进程打开的会话不写，所以也不会被读到。
- 模型用 `read` 看它，用 `radian exec --export` 已有的会话导出或 `read` 会话 journal 的只读视图看细节：细节的读法与 `sessions.md` 的现有规则一致，跨工作区的会话不可见。索引段注入时在 `MEMORY.md` 之后提一句「过去的会话在 SESSIONS.md」，不注入 `SESSIONS.md` 本身。
- `SESSIONS.md` 是低可信数据（不变量 4）；模型不能写它（`write`/`patch` 对它拒绝，记忆目录的允许规则排除这个文件名）；用户删文件就是清空，宿主下次会话结束时重建一行。
- 上限 `[memory] sessions_index_max_lines`（默认 1000）：超过时宿主丢最旧的行。
- 不改 `radian --list`、TUI 的会话列表与全文搜索。

## 验收

- 一个会话结束后 `SESSIONS.md` 多一行，字段与会话 journal 一致；当前进程打开的会话不在文件里；另一个工作区的会话不在这个项目的文件里。
- 模型在新会话里 `read` 到它，并能用会话 id 读到那段对话的导出；`write` 到 `SESSIONS.md` 被拒。
- 1001 个会话后文件是 1000 行，最旧的一行没了。
- 手册 `memory.md` 与 `sessions.md` 各写一段（一处事实，另一处链接）；`configuration.md` 加 `memory.sessions_index_max_lines`；ADR 0021 在 docs-only commit 里写进会话索引文件，并删掉本文件。
