# memory：记忆老化，先记 last_used，再定自动归档

- 状态：decided（第 1、2 条，随 `memory-files.md` 一起做；第 3 条自动归档的天数等有 `last_used` 数据再定）
- 涉及 ADR：0021（文件存储落地后的 D3）
- 来源：`research/2026-10-31-competitor-memory.md` §2.3（Codex）、§2.5（Copilot）

## 背景

对手里只有两家做了老化：Copilot 的记忆默认 90 天过期（仓库 owner 可设 7 到 180 天），被点赞重置时钟，被检索到不重置；Codex 的每条提炼记 `usage_count` 与 `last_usage`，`max_unused_days` 到期删除，合并时按使用次数再按新近度排序。Claude Code 没有过期，只靠索引上限逼模型修剪，和模型「发现错的就删」的规则。本会话自己的 Claude 记忆目录是证据：111 个文件，索引到上限一半，旧交接只能手动标「superseded」。

## 待决问题

- 自动归档的天数 N，以及归档是移到 `archive/` 子目录还是只从索引里摘掉。等有 `last_used` 数据再定。

## 设计（第一步已定，可与 memory-files 一起做）

1. 宿主在模型 `read` 一个记忆文件时把它的 frontmatter `last_used` 更新为当前时间（不计 `list`/`show` 这类用户命令）。
2. `radian memory stale [--days N]`（默认 30）列出 `last_used` 早于 N 天、或从未被读过且 `created` 早于 N 天的文件。
3. 自动归档留到第二步：有数据后定 N，宿主在会话启动时把到期的文件移到 `archive/`，索引行一并移走。

## 验收

- 模型 `read` 一个记忆文件后，该文件的 `last_used` 是本次时间，文件其余字节不变。
- `stale --days 7` 对一个 8 天前读过的文件列出它，对昨天读过的不列。
