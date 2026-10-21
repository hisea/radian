# `/goal`：续跑引擎第三步，持久目标 + `goal` 工具 + 预算

- 状态：decided＼�在 `/loop` 的重投路径上加目标；完成判定走工具。2026-10-21：系统提示块只放目标，进度只在续跑消息）
- 涉及 ADR：新 ADR（续跑引擎：loop 与 goal）；0018（压缩不摘要目标，确定性补注）、0036（系统提示的上下文区）、0011、0005（meta）、0043 §4（工具数 +1）、0037 决定 5（唤醒速率闸门与到顶可见）、0034（herdr `blocked`）、0006/0008（新 journal 记录与 schema fixture）、0022（fork）
- 来源：`docs/research/2026-10-20-peer-slash-commands.md` §2.2；总报告第 19 条；依赖 `tui-loop-continuation.md` 先落地

## 背景

Codex 官方 `/goal`（set/edit/pause/resume/view/clear，4 000 字符）与 omp `/goal`（另有 token 预算）都是「目标常驻、轮末自动续跑、完成即停」。radian 的 dogfood 多次出现「只查不改」和 resume 后要人说「继续」。目标模式复用 `/loop` 的轮末决策点，只换「投什么」和「何时停」。

## 决定（已定）

**命令**：`/goal <objective>` 设置或替换（上限 4 000 字符）；`/goal` 显示状态、进度、已用预算；`/goal pause`、`/goal resume`、`/goal drop`；`/goal budget <N turns | <dur> | <N> tokens>`。不做 omp 的 `/guided-goal`。

**持久化**：目标、状态（active/paused/done/budget-exhausted）、最近进度与预算是 durable journal 记录（新记录种类，走 0006 与 schema fixture）。目标文本**不进压缩摘要**，每次请求确定性地注入到系统提示的上下文区（0036）的一个固定「当前目标」块；**块里只有目标，不放进度**＼�：系统提示是缓存前缀的最前段（0015/0018），进度每轮都变，放进去会让每次 `goal progress` 都使整段前缀失效；目标文本稚定，放前缀安全。最近进度只出现在轮末的续跑消息里（见下），它在消息尾部，变动不影响缓存，且每轮从 durable 记录重新生成，压缩也丢不了。

**投什么**：轮末若目标 active 且本轮模型没有报 `done`，宿主投一条固定句式的续跑消息（「继续推进目标；完成就调用 goal done」+ 目标 + 最近进度），`source = User`，meta `continuation = "goal"`。与 `/loop` 同一路径、同一 Esc 与模型错误语义。

**`goal` 工具**（0043 §4 工具数 +1，和 `job {id,op}` 同风格）：`goal { op: "progress" | "done" | "wait", note?, secs? }`。`progress` 记一条进度（durable，替换「最近进度」）；`done` 把目标置为 done，续跑停止，TUI 通知；`wait secs` 把下一次续跑延后，取用户 `--every` 与 `secs` 的较大者，上限 1 小时。这是 Claude Code 自驻 `/loop` 与 pi-loop `NEXT:` 在 radian 里的对应物：延迟由模型通过工具给，不是另一种模式。工具在非目标模式下调用返回「no active goal」的工具结果错误。

**何时停**：`done`；预算到顶（轮数、时长、token 三选任意组合，缺省轮数与时长上限落地时按 dogfood 量，候选 30 轮 / 2 小时）；`pause`/`drop`；模型错误（暂停并通知）。到顶时按 ADR 0037 决定 5 唤醒闸门到顶的方式可见：TUI 通知、herdr 报 `blocked`、journal 标明。

**resume 与 fork**：`--resume` 后 active 目标显示为 paused 并在启动行提示 `/goal resume`，不自动续跑（第 19 条的「一键续跑」）。`/fork` 的子会话带目标记录但状态为 paused。

**待决（不阻塞 TUI 部分）**：`radian exec -p --goal <objective>` 是否允许，以及与 ADR 0044 里 `-p` 等后台 job 的退出链如何合并。owner 另定。

## 验收

- mock provider：设目标后一轮未报 done → 第二轮请求含续跑消息且系统提示有目标块；`goal progress` 之后系统提示字节不变（目标块不含进度），续跑消息含新进度；模型调用 `goal done` → 不再续跑且有通知；`goal wait 600` → 下一次续跑延后 600 s（可注入时钟）。
- 压缩后目标块仍在系统提示、不在摘要里；`journal-selfcheck` 绿。
- 预算 `2 turns` 恰好两轮后停，herdr 纯函数输出 `blocked`。
- `--resume` 后状态 paused、不自动开轮；`/goal resume` 后续跑。
- schema fixture 更新并通过；`/help` 列出 `/goal`。
