# 删掉持久记忆

- 状态：decided
- 涉及 ADR：0021、0006、0008、0012、0015、0020、0035、0036、0038、0039、0043
- 来源：2026-11-21 精简调查；owner 2026-11-21 决定删

## 背景

持久记忆（`radian-host/src/memory/` 约 209 KB，`radian-memory` crate 约 110 KB 含测试，ADR 0021，`docs/user/memory.md`）默认开启：每个会话把 `MEMORY.md` 索引注入系统提示，并有 `/memory` 十几个子命令、写入守卫、权限里的内建放行规则。之前的调查显示它几乎没人用；pi 没有这个功能，项目约定放在 `AGENTS.md`（ADR 0036）。

## 决定（已定）

- 删掉 `radian-memory` crate、`radian-host` 的 `memory/` 与 `transcript/memory.rs`、`/memory` 命令及其卡片、`[memory]` 配置、`memory.md`。
- 系统提示不再注入记忆索引；新会话不再写 `MemoryInjected`。
- **旧数据照常可读**：旧 journal 里的 `MemoryInjected`（`critical: false`）和 `origin.kind = "memory_addition"` 的消息仍能打开、重放、导出；ADR 0006 保留这两种形状的读取规则，写明"本版本不再写"。
- `[memory]` 和它下面的键按退役键处理：一行迁移警告后忽略，不拒绝配置文件；`configuration.md` 写明 inert。
- 用户目录 `~/.radian/memory/` 不删不动；文档说明它不再被读取，可以手动删除。
- 权限里"写入记忆目录内建放行"的规则（ADR 0012）、沙箱的记忆目录 deny 项（ADR 0020、0039）、`/context` 的记忆类别（ADR 0015）一并删掉。
- ADR 0021 改为 `Status: removed`；其余 ADR 删掉记忆相关条款，按快照写法改写。
- `memory-aging.md`、`memory-provider.md`、`memory-shared-tier.md` 三个 backlog 条目随本决定作废，已在本条目建立时删除。

## 验收

新会话的系统提示里没有记忆段；带 `[memory]` 的旧配置只多警告；含 `MemoryInjected` 的旧会话能 `--resume`、`--export`，`journal-selfcheck` 通过；workspace 里不再依赖 `radian-memory`。
