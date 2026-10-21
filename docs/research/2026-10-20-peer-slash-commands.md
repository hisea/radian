# 同行的斜杠命令与交互功能：值得收进 radian 的候选

日期：2026-10-20。目的：逐条对照 opencode、Codex CLI、omp（oh-my-pi）以及它们的上游（pi、Claude Code）的交互层命令，找出 radian 还没有、而且和 ADR 不冲突的功能。只看交互层（斜杠命令、按键、排队/打断），不看工具集和 provider。

## 0. 出处与版本（都核对过）

| 来源 | 版本 / 日期 | 看了什么 |
|---|---|---|
| opencode | v1.19.3（2026-10-16 发布） | `opencode.ai/docs/tui`、`keybinds`、`agents`、`commands`；源码 `packages/tui/src/mini/footer.prompt.tsx`、`packages/app/src/session/composer/queue.ts`（`dev` 分支） |
| Codex CLI | rust-v0.163.0（2026-10-15） | 官方命令页 `developers.openai.com/codex/cli/slash-commands`（现跳转到 `learn.chatgpt.com/docs/developer-commands?surface=cli`） |
| omp | 本机 18.10.4（最新 v18.12.3，2026-10-19） | 本机源码 `~/.bun/install/global/node_modules/@oh-my-pi/pi-coding-agent/src/slash-commands/builtin-{lifecycle,control,modes,session,collaboration,marketplace}.ts`、`modes/interactive-mode.ts`、`docs/slash-command-internals.md`、`docs/magic-keywords.md` |
| pi | 本机 1.3.0（`@earendil-works/pi-coding-agent`） | bundle 里的内建命令名 |
| Claude Code | 文档 `code.claude.com/docs/en/interactive-mode`（2026-10） | `/btw` 的原始语义、排队语义 |
| pi 扩展 | `j3ssie/pi-loop`、`dbachelder/pi-btw` | `/loop` 与 `/btw` 的另一种实现 |

radian 今天的命令（`crates/radian-tui/src/command.rs`）：`/help` `/compact` `/fork` `/theme` `/sessions` `/export` `/memory` `/jobs` `/usage` `/model` `/login` `/logout` `/auto`（`/yolo`）`/access` `/skills` `/thinking` `/skill:<name>`。Enter 在空闲时是 `followup`、在回合进行中是 `steer`（ADR 0011）；`!`/`!!`（ADR 0033）；`@file`；Ctrl-F 搜索；`radian attach` 只读看别的会话（ADR 0043 §7）。

## 1. 各家内建命令全表（去掉 radian 已有的和纯平台相关的）

- **opencode**（15 条 + 自定义 markdown 命令）：`/connect` `/compact` `/details` `/editor` `/exit` `/export` `/help` `/init` `/models` `/new` `/redo` `/sessions` `/themes` `/thinking` `/undo`。按键里另有 `prompt.queue`（排队投递）、`session_interrupt`、`session_timeline`、`prompt_stash`、子会话导航（`<leader>down`/`left`/`right`/`up`）、agent 切换（Tab）、variant 切换（Ctrl-T）、`attention` 通知。
- **Codex CLI**（56 条）：有而 radian 没有的：`/new` `/clear` `/rename` `/archive` `/delete` `/copy` `/diff` `/goal` `/ps` `/stop` `/side`/`/btw` `/raw` `/review` `/recap` `/status` `/mention` `/plan` `/permissions` `/approve` `/memories` `/import` `/init` `/mcp` `/hooks` `/agents` `/subagents` `/worktree` `/cd` `/statusline` `/title` `/keymap` `/vim` `/experimental` `/fast` `/warnings` `/debug-config`。另有「回合进行中输入斜杠命令按 Tab 排到下一回合」。
- **omp**（约 60 条）：lifecycle：`/new` `/fresh` `/clear` `/drop` `/shake` `/handoff` `/resume` `/btw` `/tan` `/omfg` `/retry` `/rename` `/move` `/add-dir` `/remove-dir` `/dirs`；control：`/force` `/pause` `/live`；modes：`/plan` `/plan-review` `/vibe` `/goal` `/guided-goal` `/loop` `/queue` `/switch` `/fast` `/computer` `/vision` `/prewalk` `/security` `/settings` `/setup`；session：`/todo` `/session` `/jobs` `/usage` `/stats` `/changelog` `/hotkeys` `/tools` `/context` `/extensions` `/agents` `/branch` `/fork` `/tree` `/mcp`；collaboration：`/advisor` `/export` `/dump` `/share` `/collab` `/join` `/leave` `/browser` `/copy`；marketplace：`/marketplace` `/plugins` `/reload-plugins`。
- **pi**（基线）：`/new` `/resume` `/compact` `/model` `/models` `/thinking` `/export` `/share` `/session` `/settings` `/hotkeys` `/changelog` `/login` `/logout` `/copy` `/name` `/fork` `/tree` `/quit` `/reload`。

## 2. 重点功能的精确语义（四家不一样，不能笼统地说「加个 /btw」）

### 2.1 旁路提问 `/btw`

| 实现 | 看到什么上下文 | 有没有工具 | 跑在哪里 | 结果去哪 |
|---|---|---|---|---|
| Claude Code（原型） | 到目前为止的对话（不含正在写的回复）+ 最近 20 条旁路问答 | 没有；写了工具调用也不执行 | 独立请求，主回合不受影响；前缀缓存热时很便宜 | 覆盖层，永不进历史；`f` 可叉成后台子代理 |
| Codex `/side`/`/btw` | 「ephemeral side chat」 | 文档未说 | 文档未说 | 不进主对话 |
| omp `/btw` | 当前会话的模型与上下文，**含正在流式输出的半截文本** | 文档未说 | 独立请求；**一次只允许一个**，忙时拒绝不排队 | 覆盖层；按会话存成私有文件；空闲时可 `b` 升级成分支 |
| pi-btw 扩展 | 从主分支克隆一个真子会话 | `read` `bash` `edit` `write`（`/btw:ask` 只读） | 独立子会话，主代理忙时也能跑 | 隐藏条目；`/btw:inject`/`:summarize` 才回主线 |

对 radian 最合适的是 Claude Code 的形状：一条额外的模型请求，输入 = 当前 transcript 快照 + 问题，不带工具，答案只在屏幕上，不写 transcript、不写 journal（live 事件，ADR 0010）。前缀和主回合完全相同，所以缓存命中（ADR 0018）。实现点在 `radian-host`/TUI 层，不碰 `agent-core` 的回合循环。要定的事：主回合在飞时允不允许并发第二个请求（omp 允许，Claude Code 允许）；用哪个模型（缺省同一个；omp/pi-btw 可单独配）。

### 2.2 `/loop`：三种完全不同的东西叫同一个名字

1. **omp `/loop [count|duration] [prompt]`**：一个**开关**。开着的时候，下一条提交的 prompt 在每次 yield 后原样重投；可限次数或时长；Esc 取消本次迭代但不退出模式；再 `/loop` 关掉。没有定时器，不等间隔。
2. **pi-loop 扩展 / Claude Code `/loop`**：**定时器**。`/loop 5m <prompt>` 每 5 分钟投一次；无间隔则自驻（agent 在最后一条消息里写 `NEXT: 10m`、`LOOP: done`）；`--at 09:00`、`--times N`、`--watch <path>`、`--each <file>`、`--cmd`。只在回合之间触发，多次触发合并成一次，会话级别、7 天过期。
3. **Codex `/goal` / omp `/goal`**：**持久目标**。Codex：set/edit/pause/resume/view/clear，4 000 字符上限；omp：另有 token `budget`，并有 `/guided-goal` 让 agent 先采访你再设目标。回合结束后按目标自动续跑。

radian 现状：没有 1 也没有 2；3 已经在总报告第 19 条「持久 todo + `/goal`」（idea，无文件）。1 最便宜：回合结束时若 loop 开着就把 loop prompt 以 `followup` 入队（ADR 0011 L5 的空闲唤醒路径已经存在），次数/时长在 TUI 侧数。要定的事：重投的消息 `source` 算什么（ADR 0011 L2），因为 `wake-cap-unattended.md` 的闸门按「用户消息」清零；按用户来源算就不会被闸住，按 host 来源算就会。2 需要定时器和 `exec -p` 下的行为，和 `wake-cap`/`exec-waits-for-jobs` 一起看；外部定时器（herdr、cron）今天已经能驱动 radian。

### 2.3 排队投递（和 `steer` 的关系）

| 实现 | 回合进行中按 Enter | 另一种投递 | 队列可见/可改 |
|---|---|---|---|
| radian | `steer`（安全点插入当前回合） | 没有 TUI 入口投 `followup` | 不可见 |
| Claude Code | **排队**（工具批次结束后或回合结束后送达） | Ctrl-Enter 立刻送（能后台的工作转后台） | 灰显列表；Up 取回编辑 |
| opencode | 缺省 `steer`；忙时可切 `queue` | 命令面板 `Queue prompt` | 面板里可编辑、重排、撤回、把排队项改成 steer |
| omp | steer | `/queue <message>` 等 yield 后投 | — |
| Codex | — | 斜杠命令按 Tab 排到下一回合 | — |

radian 的 inbox 已有 `followup` 和 `steer` 两条 lane，缺的只是 TUI 入口和可见队列。最小做法：`/queue <msg>`（omp 形状）+ footer 一行「queued: N」+ 空输入时 Up 取回。opencode 的「忙时面板里改投递方式」是更完整的版本。

### 2.4 `/tan`（omp）：用户发起的后台子代理

`/tan <work>` 从当前会话克隆一个子会话跑旁支工作，走 `asyncJobManager`，结果以后台任务的方式回到主线。radian 的 `subagent` 工具是模型发起、阻塞的（ADR 0043 §2）；用户发起的版本 = 从 TUI 起一个子会话进程，结束后像后台 job 一样以 System message 走 ADR 0037 的通知 lane。中等成本，依赖 0043 §2 和 0037。

## 3. 建议收进来的（按收益/成本）

### 第一档：便宜、所有同行都有、不碰 ADR 决定

| 候选 | 谁有 | radian 侧的做法 | 涉及 ADR |
|---|---|---|---|
| `/queue <msg>` + 可见队列 | opencode、omp、Claude Code、Codex（Tab） | TUI 入口投 `Inbox::followup`；footer 显示条数；Up 取回 | 0011、0027 |
| `/btw <question>` | Claude Code、Codex、omp、pi 扩展 | 一次无工具请求，同一 transcript 前缀，只出覆盖层/notice，不落盘 | 0010、0018、0023 |
| `/new` | 四家都有 | 原地开新会话；今天 `/sessions` 不原地切换，`/new` 和它共用「切换会话」这一块 | 0022、0023 |
| `/context` | omp、Claude Code、Codex `/status` | 按类别（系统提示、AGENTS.md、工具定义、历史、工具结果）给出估算；ADR 0015 的估算器已存在，footer 只显示总百分比 | 0015 |
| `/copy` | Codex、omp、pi | 复制最近一条 assistant 文本到剪贴板，走已有的 `clipboard = osc52` | 0027 |
| `/retry` | omp | 模型错误结束回合后（ADR 0017）重投上一条用户消息，不用再打一遍 | 0017、0011 |
| 终端提醒（bell） | opencode `attention`（缺省关） | ask/审批/回合结束时发 BEL，配置缺省关；对 herdr 之外的裸终端用户有用 | 0027、0024 |

### 第二档：要先定设计

| 候选 | 要定的问题 |
|---|---|
| `/loop [N\|duration] [prompt]`（omp 形状） | 重投消息的 `source`；和 `wake-cap-unattended.md` 的闸门怎么互动；`exec -p` 下是否允许 |
| `/goal`（Codex/omp 形状） | 已是总报告第 19 条 idea；Codex 现在官方有了，证据更足；建议升成有文件的 backlog 条目，和 `/loop` 一起定 |
| `/tan <work>` | 用户发起的子会话进程 + 结果走 0037 通知 lane；是否共享父会话历史（omp 克隆，radian `subagent` 从空白开始） |
| `/editor` | 用 `$EDITOR` 编辑长输入（opencode `/editor`、Codex `/vim` 之外的做法）；全屏 TUI 要挂起再恢复（ADR 0009） |
| `/init`、`/review` | 不写代码：各做成一个随二进制捆绑的 skill（ADR 0025），`/skill:init`、`/skill:review`；`/review` 内部用 `subagent` 并行 |
| `/mcp`、`/hooks` 列表 | 等 `mcp-servers-json.md`、ADR 0045 落地后顺手加，列出配置来源与状态 |

### 第三档：不做，或 owner 已有方向

- **`/plan`、agent 切换（Tab）、`@agent`、`/vibe`、`/agents` hub**：定了「一种会话，没有 /mode 和 agent kinds」（ADR 0043 §1）。radian 的最近似物是 `/access` 的只读档（ADR 0038）。
- **`/undo` `/redo`（opencode）、`/branch` `/tree`（omp、pi）**：要文件快照或从历史中点分叉；`/fork` 只在最后一个回合边界（ADR 0022）。记 idea，不排。
- **`/share` `/collab` `/join`**：出网、要 relay；和 ADR 0013 的缺省不出网相反。herdr 的 `attach` 覆盖本地场景。
- **`/worktree` `/cd` `/add-dir` `/move`**：radian 的 workspace 是会话固定的（沙箱与信任门都按它算，ADR 0016、0031）；worktree 由 herdr 管。
- **`/force <tool>`、`/advisor`、`/omfg`（TTSR 规则）、`/prewalk`、`/live`、`/computer`、`/pets`**：各家特色，不是缺口。
- **`/shake`**：对应 `context-pruning.md`，不另开。
- **`/import`（Codex 导入 Claude Code/Cursor）**：对应 `import-claude.md`，不另开。
- **`/ps` `/stop`**：就是 radian 的 `/jobs`。

## 4. 未核实

- Codex `/side`/`/btw` 的上下文范围和是否带工具：官方页只有一句描述。
- omp `/tan` 的结果如何回到主线（只读了控制器头部和 `asyncJobManager` 的使用，没跑）。
- opencode 忙时 Enter 缺省是 `steer` 这一点来自 `footer.prompt.tsx` 的 `submitPrompt` 缺省值；用户配置能否改缺省没查。
