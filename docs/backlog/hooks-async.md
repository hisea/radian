# hooks：`async` 后台 hook

- 状态：decided（可以派发；排在 `hooks-permission-request.md` 之后，见 `hooks-stop-and-feedback.md` 的顺序说明；依赖那一条的 `additionalContext` 解析与 `background_tasks` 字段）
- 涉及 ADR：0045（§1 明文排除 `async`、§5 `PostToolUse` 同步写入）、0037（决定 5 唤醒闸门、完成通知）、0011（`inject` 与 `followup` lane、L2 来源）、0046（import-claude 现在把 `async` 条目丢弃）
- 来源：`research/2026-10-24-claude-code-hooks.md`；Claude Code hooks 参考页 `Run hooks in the background` 一节；radian dogfood 里 post-edit lint 同步跑拖慢回合的观察

## 背景

0045 §5 规定 `PostToolUse` 同步跑、结果写在该工具结果之后再返回。post-edit lint 和测试这类 hook 因此每次编辑都阻塞回合；重一点的检查（整包测试）没人敢配。

Claude Code 的 `async: true` 让 command hook 在后台跑，模型不等它；进程退出后 stdout JSON 里的 `additionalContext` 在下一回合送达；`asyncRewake: true` 的 hook 退出 2 时立即唤醒空闲会话。async hook 不能阻止或改写任何东西（它要控制的动作已经发生）。

送达沿用 0037 已有的两条 lane，不需要新概念：结果走 `inject`（不唤醒），`asyncRewake` 退出 2 走 `followup`（计一次 wake，过决定 5 的闸门）。

## 决定（已定）

字段名沿用 Claude Code（owner 2026-10-29）。

### 1. 字段与事件

- hook 表新增 `async`（bool，默认 `false`）与 `asyncRewake`（bool，默认 `false`）。`asyncRewake = true` 隐含 `async = true`。
- 只 `PostToolUse` 与 `Stop` 接受。`PreToolUse` 与 `PermissionRequest` 上写了 `async` 或 `asyncRewake` 的 hook 跳过，警告一行（同"子集外条目"的规则；那两个事件的意义就是在调用前作决定）。
- `async` 从 0045 §1 的子集外清单里拿掉（`NON_COMMAND_HANDLER_KEYS` 两处与 import-claude 的丢弃理由同时改）。

### 2. 运行

- 启动方式、沙箱与 env 和同步 hook 相同（`workspace` 档同一套沙箱）。stdin 也相同。工具结果或 `Stop` 链立即继续，不等它。
- `Stop` 的同步链（exit 2、validator、续跑合并）只包含同步 hook；async `Stop` hook 不参与本回合的续跑合并，也不计入 `hooks-stop-and-feedback.md` §3 的空转计数。
- 超时：`timeout` 照常生效；async hook 没写 `timeout` 时默认 600 s（同步仍是 60 s）。超时杀进程组。

### 3. 结果怎么进上下文

- exit 0 且 stdout JSON 有 `hookSpecificOutput.additionalContext`：走 `inject`（`System` 来源），不唤醒，下一次模型调用之前进上下文（忙时本回合的下一步，闲时下一回合）。正文 `[<事件> hook] <文本>`，2000 字符截断。
- `asyncRewake` 且 exit 2：stderr 为正文，走 `followup`。空闲时唤醒一个回合，计一次 wake，与 `Stop` 续跑共用 `CommandHooks` 的那扇闸门；闸门满时退化为 `inject`，出与后台通知同样的三处可见信号（`degraded`、TUI 一行、herdr `wake budget`）。不是 `asyncRewake` 的 hook exit 2：按 `additionalContext` 同样走 `inject`，正文是 stderr。
- 其他退出码、超时、无法启动：不失败关闭（它控制的动作已经发生），只警告一行，并在 `hooks-stop-and-feedback.md` §5 的最近运行表里记一条。
- 返回的 `permissionDecision`、`updatedInput`、`updatedToolOutput`、`decision`：忽略，警告一行。

### 4. 不是 job

- async hook 不进 `Jobs` 注册表：不出现在 `/jobs` 与 `job` 工具里，模型不能 `wait` 或 `kill` 它（不是模型起的）。它在 `/hooks` 页的"正在运行"一节可见，并按 `hooks-stop-and-feedback.md` §1 列在 `Stop` 的 `background_tasks` 里（`type: "hook"`）。
- 内建 intent-stop validator 把正在跑的 async hook 当作后台 job：有 async hook 在跑时不触发（0045 §3 的"无后台 job"条件）。

### 5. 生命周期（单独一个 task）

- 取代：同一条 hook（命令与 `args` 相同）又被触发时，还在跑的那次被杀掉（进程组），只留最新一次的结果；被杀的那次不送达任何内容，最近运行表记 `cancelled`。新编辑让旧 lint 结果过期，这是 post-edit 检查的常见场景。
- 上限：每个会话同时最多 8 个 async hook 进程。满了时新的一次不启动，警告一行，最近运行表记 `failed to start`。
- 回合被取消：已在跑的 async hook 不受影响（它属于已经发生的动作）。
- 会话结束（TUI 退出、`exec` 结束、`/new` 换会话）：全部杀掉，不送达。`exec -p` 不为它们等待。
- 送达时会话正在压缩或替换 transcript：按 inbox 的既有规则排队，不丢。

## 验收

- 配了 `async: true` 的 `PostToolUse` hook，工具结果立即返回（测试里 hook `sleep 2`，结果在 1 s 内返回）；hook 退出后其 `additionalContext` 在下一次模型调用之前进上下文；同回合连续编辑三个不同文件起三次不同命令，各自一条。
- 同一条命令在第一次未结束时又触发：第一次被杀、只有第二次的内容送达。
- `asyncRewake` 退出 2 在会话空闲时唤醒一个回合，计入闸门；超闸门时退化为 `inject`，三处信号都在。
- async hook 返回的 `permissionDecision`、`updatedInput` 被忽略并警告一行；`PreToolUse` 上的 `async` 条目跳过并警告。
- `workspace` 档下 async hook 子进程在同一沙箱内（写工作区外失败）。
- 第 9 个并发 async hook 不启动并警告；会话结束时还在跑的 hook 进程组被杀（测试检查 pid 不存在）。
- async hook 在跑时 intent-stop validator 不触发；`Stop` 的 `background_tasks` 列出它。
- import-claude 不再丢弃 `async`/`asyncRewake` 条目（`PreToolUse` 上的仍丢弃并报告）；`docs/user/hooks.md` 写明两个字段、默认值、哪些事件接受、默认超时、上限与取代规则；ADR 0045 §1 与 §5 在同分支的 docs-only commit 里改成新快照。
