# memory：MemoryBackend trait、外部命令 backend、离线 capture

- 状态：idea
- 涉及 ADR：0021、0013、0031
- 来源：`research/2026-10-18-area-deep-dive/9-long-tasks-memory.md` §6.5

## 背景

模型侧的写工具已经删除，`memory_search` 改为超出注入预算才挂载（pi/memory-tools，ADR 0021 D5/D7）。读靠注入，写靠用户命令。剩下的是两件还没定的事：记忆从哪来可以替换，以及没有人手写时记忆怎么长。

## 待决方向（lead 整理，待定）

- **`MemoryBackend` trait**（宿主级，不进 radian-core；名字避开"provider"，那是模型路由）：`fixed(query, budget) -> Vec<Item>`、`addition(turn_text, exclude, budget) -> Vec<Item>`、可选 `capture(evidence) -> Vec<Candidate>`、可选 `search`、`status`。返回结构化记录，不返回提示词文本；渲染、低可信说明、分段、预算裁剪、`MemoryInjected` 账本全部留在宿主。接缝就是今天的 `MemoryInjection`（`MemoryHooks::before_model_call` 与 driver 的 `TurnAdditions` 闭包都只调它）。backend 出错按不变量 2 吞掉，不能做成 `Hooks`（Hooks 出错结束回合，ADR 0017）。
- **内置 `JsonlBackend`** = 今天的 `MemoryService` + `injection::plan`，纯重构。
- **`CommandBackend`**：每次调用一个一次性子进程，stdin JSON（`{"v":1,"op":"fixed|addition|capture|search|status",…,"budget_tokens":N}`）→ stdout JSON（`{"items":[…]}`）；超时（inject 2 s、capture 30 s）、stdout 上限 64 KiB、非零退出一律空结果 + 一次 warn；跑在宿主侧沙箱外；只允许用户级配置（项目级 `.radian/config.toml` 本来只读 `permissions.profile`）；它收到用户每轮原话，是一条出网路径，要进 ADR 0013 的清单。和外部命令 hook 共用同一个子进程 runner，等那份 ADR 一起设计。
- **capture 的形态**：照 Codex/omp-local，离线从**已提交**的 journal 提炼（ADR 0021 D9 / 0019），不是轮末多打一次模型。入口 `radian memory consolidate`，可选的启动后台触发默认关；选材按空闲时长、最大年龄、每次数量上限，当前进程打开的会话排除；领取/重试状态放 `sessions/index.db`；写入经 `MemoryService::create`，`source` 新增 `Extracted`，`Unverified`；候选过一遍 `heuristic-secret` 检测；用 `MemoryInjected` 的 id 剔掉"注入过又被复述"的内容。不做 Codex 的阶段 2 整合：JSONL 记录自带 key 与 revision，重复靠 `KeyConflict` 挡。
- 不接压缩（ADR 0018 记忆与压缩独立）。

## 待决问题

- capture 用哪条路由的模型（ADR 0026 的小模型指定）；用量按 ADR 0007 单独记账，不进任何会话的预算基线。
- 契约测试：同一套用例跑 `JsonlBackend` 和一个 fixture 脚本的 `CommandBackend`（固定段两次调用字节相同、预算从不超出、exclude 生效、出错/超时/超大输出三种都注入空且只 warn 一次、候选与用户记录同 key 时被 `KeyConflict` 拒绝）。

## 验收

写成 ADR 0021 D8 的替换文字（"不预先公开插件 trait"改为这个 trait）加 ADR 0013 清单一项，再派发。
