# 长时间运行的 shell 命令：后台执行与完成通知 —— 调研

对象：Claude Code、omp（oh-my-pi）、DeepSeek Harness（dsh）、pi、opencode。目的：为
ADR 0037（后台命令）提供事实与出处。**只读**：npm tarball、公开仓库、随包文档与 schema；没有运行
任何一个 agent，没有读 `~/.pi`、`~/.claude`、`~/.codex`、`~/.omp`、`~/.radian` 或任何用户数据、
凭据目录。临时解包目录在本文写完后删除。

## 0. 出处

| 前缀 | 来源与版本 |
|---|---|
| `cc/` | npm tarball `@anthropic-ai/claude-code@2.1.300` 与 `@anthropic-ai/claude-agent-sdk@0.3.300` 的 `sdk-tools.d.ts` / `sdk.d.ts`（2.1.300 的 `claude` 已是原生二进制，不再带 `cli.js`，所以用 SDK 的类型定义作为工具契约的权威） |
| `omp18/` | npm tarball `@oh-my-pi/pi-coding-agent@18.10.4`（带 `src/`） |
| `dsh/` | 公开仓库 `deepseek-ai/deepseek-harness`，默认分支 `master`，shallow clone（`packages/jobs/`） |
| `pi12/` | npm tarball `@earendil-works/pi-coding-agent@1.2.0`（只有 `dist/`、`docs/`、`examples/`） |
| `oc/` | 公开仓库 `anomalyco/opencode`（`github.com/sst/opencode` 301 跳到这里），分支 `dev`，commit `4397ee4` |
| `rad/` | 本仓库 `origin/main`（`ea724f9`） |

## 1. 五个 agent 各怎么做后台命令

### 1.1 Claude Code

- **启动**：`Bash` 工具有 `run_in_background?: boolean`（`cc/sdk-tools.d.ts` 的 `BashInput`）。带它时
  `timeout` 是「命令可在后台跑多久才被停掉」，**默认 1 800 000 ms（30 min）、上限 7 200 000 ms
  （2 h）**；前台命令的 `timeout` 上限是 600 000 ms（10 min）。人还可以在终端里按 **Ctrl+B** 把一条
  正在前台跑的命令转后台；SDK 的 `backgroundTasks(toolUseId?)` 是同一件事的控制请求（`cc/sdk.d.ts`
  约 3238–3248：「Each blocking tool call returns immediately with a 'running in the background'
  tool_result and the turn continues」）。`CLAUDE_CODE_DISABLE_BACKGROUND_TASKS` 可以整个关掉。
- **拿输出**：`BashOutput`（`cc/sdk-tools.d.ts` 的 `BashOutput`）带 `backgroundTaskId`、`stdout`、
  `stderr`、`rawOutputPath`（大输出的原始文件）、`backgroundedByUser`（Ctrl+B）、`timedOutAfterMs`
  （命中超时被自动转后台）。也就是说：结果对象既能描述一次已完成的调用，也能描述一个还在跑的后台任务；
  读它的输出另有工具（`BashOutput` 工具）按 id 取。
- **知道结束**：一条 `task_notification` 系统消息（`cc/sdk.d.ts` 约 6092）：`type: 'system'`、
  `subtype: 'task_notification'`、`task_id`、`tool_use_id`、`status: 'completed' | 'failed' |
  'stopped'`、`output_file`、`summary`，以及可选的 `resource_links`。它是**会话里的一条系统消息**，
  模型下一轮就会看到；「you will be notified when one completes」（`Task` 工具的
  `run_in_background` 文档，`cc/sdk-tools.d.ts` 约 794）。
- **取消**：`TaskStop`（`task_id`；`shell_id` 已废弃），`stopTask(taskId)`；停止会再发一条
  `task_notification`，`status: 'stopped'`。
- **进程组与清理**：SDK 类型里没有暴露进程组细节。可核实的两条边界：`backgroundEndsWithFinalResponse`
  ——一个同步子 agent 的、或一个不再接收输入、也不等后台命令的 headless 会话里的后台命令，会在**该
  caller 的最终回答时被终止**，「so no completion notification can follow」；以及 `reason:
  'worker_restart'`——worker 进程重启后发现任务成了孤儿，`status: 'stopped'`。会话结束时如何清理还在跑
  的进程，类型定义里没有写（**未核实**）。

### 1.2 omp（oh-my-pi）18.10.4

- **启动**：`bash` 工具的 `async?: boolean`（`omp18/src/tools/bash.ts`，schema 有
  `bashSchemaWithAsync` 变体，由 `async.enabled` 设置决定是否出现）。`async: true` 时命令注册成
  `session.asyncJobManager` 的一个 job（`async: { state: "running", jobId, type: "bash" }`，
  `bash.ts` 约 791）。还有一个**自动转后台**：前台等待超过阈值（`DEFAULT_AUTO_BACKGROUND_THRESHOLD_MS
  = 60_000`，`omp18/src/async/auto-background.ts`）就转后台；阈值由 `bash.autoBackground` 与
  `bash.autoBackgroundThresholdMs` 控制（`bash.ts` 的 description 渲染）。job 数有上限
  （`maxRunningJobs`，`omp18/src/async/job-manager.ts` 约 362：超过就报「Background job limit
  reached」）。
- **拿输出**：job 的进度帧（`details.async`）与 `job_output` 式的读取；结果交付走 `async-result`
  模板（`omp18/src/prompts/tools/async-result.md`）：一段 `<system-notice>`，多 job 时
  「N background jobs have completed. Resume your work using the results below.」；结果超过
  `ASYNC_INLINE_RESULT_MAX_CHARS = 12_000` 就溢出一个 artifact、正文只留预览
  （`omp18/src/session/async-job-delivery.ts`）。
- **知道结束**：job 的 promise settle → `#enqueueDelivery`（`job-manager.ts` 约 489）→ 归属
  session 的 yield queue「**idle flush injects them as a follow-up turn**」
  （`async-job-delivery.ts` 开头）。也就是说：**完成通知会自己开一轮**，模型被唤醒。还有一个 `wait`
  工具（`omp18/src/tools/wait.ts`）让模型主动阻塞等自己启动的 job/service，`WAIT_MAX_MS = 30 min`，
  等待中收到消息或 steer 就结束等待。
- **取消**：`cancel(id)`（按 owner agent 作用域，`job-manager.ts` 约 446）→
  `abortController.abort()`；`cancelAll`。job 状态 `running | completed | failed | cancelled`。
- **进程组与清理**：job 持一个 `AbortController`；`dispose({ timeoutMs: 3000 })` 在会话拆除时
  `cancelAll`（带 `ASYNC_JOB_MANAGER_SHUTDOWN_REASON`）、最多等 3 s 让 job settle、再 drain
  deliveries（`job-manager.ts` 的 `dispose`）。子进程的进程组细节在 `bash` 工具里（`setsid` 一类），
  本文没有逐行核实（**未核实**）。

### 1.3 DeepSeek Harness（dsh）

dsh 把「后台 job」做成了一整个子系统（`dsh/packages/jobs/{jobs,jobs-local,tool-jobs}`），文档最完整：

- **启动**：producer（bash、PTY 等）通过 `ctx.jobs.start()` 注册；`dsh-tool-jobs` 插件是「arms
  producers' `ctx.jobs.start()`」的那一半。每个 job 有稳定的 `<kind>-<short-uuid>` id，**归属限定在
  agent session**（其它 agent 不能看、不能停）。有并发与输出保留上限（`jobs-local`）。
- **拿输出**：`job_output(job_id, wait?, timeout_ms?)`。流式 job 只返回**上次读之后**的输出；
  final-output job 在 settle 后返回结果。每个响应以 `[status: ...]` 结尾。默认非阻塞，`wait: true`
  最多等 `maxWaitTimeoutMs`（默认 600 s；`waitTimeoutMs` 默认 30 s），超时返回 `[status: running]`
  并**留着 job 继续跑**。stdout 先渲染，stderr 归到一个 `[stderr]` 段；读出内存之前丢掉的输出会说明；
  job 的结果（例如子 agent 的回答）只在 settle 后的**第一次**读里带一次。
- **知道结束**：完成时归属 agent 收到一条 in-session 消息：`background job <id> (<kind>: <label>)
  finished [status: ...]. Read its output with job_output.`。**忙碌的 agent 把通知注入它的下一步**
  （「the turn cannot close while the inbox holds it」）；**空闲的 agent 被一个 follow-up turn
  唤醒**（「an unclaimed notice is a completion the model never learns about」）。模型已经自己收走的
  完成（一次 `wait` 被 settle 释放）不发通知；`job_kill` 杀掉的、以及 owner/service 拆除引起的
  settle 也跳过。`maxConsecutiveWakes` 给唤醒链设上限（超过就退化成注入；任何用户消息重置预算）；
  `completionDelivery: quiet` 让空闲 owner 也走注入。
- **取消**：`job_kill(job_id, reason?)`——请求取消，job 在**工作真正停止后**才 settle 成 `killed`；
  终态 job 返回当前快照。
- **进程组与清理**：`jobs end when their owner or the harness shuts down`（`jobs-local` README）；
  「pending notices do not survive owner disposal」（`tool-jobs` README）。

### 1.4 pi 1.2.0

**内建工具里没有后台命令**：`dist/` 里 grep 不到 `run_in_background` 一类的参数，`docs/` 也没有后台
bash 的说法。pi 有的是**扩展机制**：扩展可以 `pi.sendMessage(..., { triggerTurn: true })` 发一条
custom message，**它会开一轮**（`pi12/docs/extensions.md:104`：`before_agent_start` 的
`event.messageType` 是 `"custom"`「for `pi.sendMessage(..., { triggerTurn: true })`」）；custom
message 的形状是 `{ role: "custom", customType }`（`pi12/docs/message-types.md:220`）。随包的
`plan-mode` 扩展的注释直接点名这个用法：「Runs started by custom messages, such as **background
task notifications**」。所以 pi 的后台命令（如果有）会是一个扩展，用 custom message 把完成通知送回去
并开一轮。

### 1.5 opencode

opencode 有内建后台命令（`oc/packages/core/src/tool/plugin/shell.ts`）：

- **启动**：`shell` 工具的参数 `background?: boolean`——「Run the command in the background and
  return immediately (useful for dev servers and long-running builds). You do not need to use '&'
  at the end of the command when using this parameter.」前台默认 `DEFAULT_TIMEOUT_MS = 2 min`；
  **后台命令默认 `timeout = 0`（不超时）**。
- **拿输出**：立刻返回 `Command moved to the background (shell ID: <id>).\nOutput is streaming to:
  <file>`（`backgroundResult`），输出**流到一个文件**；结构化输出带 `shellID`、`exit`、`signal`、
  `truncated`、`timeout`。
- **知道结束**：`notifyWhenDone` 在 scope 里 fork，`jobs.wait({ id })`，然后
  `sessions.synthetic({ ... })` 注入一条**合成消息**（`shell.ts` 约 150–200），形状是
  `<shell id="<jobID>" state="completed|cancelled|error" command="<cmd>">\n<text>\n</shell>`
  （`oc/packages/core/src/shell/result.ts:45`），并 `jobs.completeBackground(notificationID)`。
  工具描述明确要求模型不要轮询：「**DO NOT poll for completion** … If you have nothing else to do,
  end your response; **you will be resumed automatically** when the command finishes.」
- **取消**：`Job.cancel(id)` → 状态 `cancelled`；job 状态 `running | completed | error | cancelled`
  （`oc/packages/core/src/job.ts`）。
- **进程组与清理**：后台结果会 persist（`persistBackground`，`job.ts` 约 235）；「Unconsumed results
  survive the start-to-wait handoff」（`job.ts` 约 174）。会话结束时的进程清理细节没有逐行核实
  （**未核实**）。

### 1.6 一张对照表

| | Claude Code | omp 18.10.4 | dsh | pi 1.2.0 | opencode |
|---|---|---|---|---|---|
| 启动 | `Bash.run_in_background`、Ctrl+B、`backgroundTasks()` | `bash.async: true`、超 60 s 自动转后台 | producer 经 `ctx.jobs.start()` | 内建没有；扩展 | `shell.background: true` |
| 后台超时 | 默认 30 min，上限 2 h | job 不受 shell 超时约束（**未逐行核实**） | producer 自定 | — | 默认不超时 |
| 拿输出 | `BashOutput` 工具按 id；`rawOutputPath` | job 进度帧 / 结果交付 | `job_output`（增量、`[status]`） | — | 流到文件 + 结果 |
| 结束通知 | `task_notification` 系统消息 | `async-result` `<system-notice>` **follow-up turn** | in-session 消息：busy 注入下一步、idle **唤醒一轮** | custom message `triggerTurn: true` | 合成 `<shell …>` 消息，**自动 resume** |
| 自动开新轮 | 是（系统消息进对话） | 是（follow-up turn） | 是（idle 唤醒；可设上限/quiet） | 是（triggerTurn） | 是 |
| 取消 | `TaskStop` | `cancel(id)` / `cancelAll` | `job_kill` | — | `Job.cancel` |
| 会话结束 | `backgroundEndsWithFinalResponse` 边界；其余**未核实** | `dispose`：cancelAll + ≤3 s | owner/harness 关闭即结束 | — | persist；其余**未核实** |

## 2. 完成通知怎么回到模型

五种做法其实只有两类：

1. **会话里的一条消息**（Claude Code 的 `task_notification` 系统消息、opencode 的合成
   `<shell …>` 消息、omp 的 `async-result` `<system-notice>`、dsh 的 in-session 通知、pi 的 custom
   message）。这条消息进对话、模型下一轮读到。
2. **要不要自己开一轮**：
   - omp：完成**自己开一轮**（idle flush → follow-up turn）。
   - dsh：**busy 注入下一步，idle 唤醒一轮**；唤醒链有 `maxConsecutiveWakes` 上限，`quiet` 时一律注入。
   - opencode：「you will be resumed automatically」。
   - Claude Code：`task_notification` 是系统消息，模型下一轮读到（是否强制开轮由产品决定，类型定义
     只说明它进对话）。
   - pi：扩展显式选择 `triggerTurn: true` 才开轮。

共同点：**没有一家让模型靠轮询拿结果**；四家的工具描述都明确写「不要 sleep/轮询，结束回答，完成时你会
被唤醒」。这与 radian 现有规则（「不要 sleep 轮询」，调研
`2026-10-14-system-prompts.md` §7 未核实第 8 条）一致。

## 3. 与 radian 已有设计的接口

- **inbox（ADR 0011）**：`followup`（`next_turn` lane，唤醒）就是「完成通知开一轮」的现成机制；
  `inject`（`next_step` lane，**不唤醒**）就是 dsh 的「busy 注入下一步」；`steer`（`next_step`，
  唤醒）不适用（它只接受 user message，`steering.rs` 的 `NotUserMessage`）。完成通知应当是
  **System 来源**（不是 `User`），所以只能走 `inject`（不唤醒）或一条新的 `followup` 变体。
  `inbox` 的 `followup` 要求显式 `source`（ADR 0011 决定 4），通知的 `source` 是 `System`。
- **durable 事件（ADR 0010）**：后台命令的**启动**（命令、id、cwd、沙箱档位）与**结束**（退出状态、
  是否被取消/超时）是重建与审计必需的 → durable；进度（已产出的字节数、当前输出）是过程量 → live，
  可丢/合并。这与 `ToolExecutionStarted`/`ToolExecutionSettled`（ADR 0002）同构：一次后台命令就是一次
  工具执行，只是它的 settle 来得晚。
- **`!` 命令（ADR 0033）**：用户亲手输入的 `!` 已经是「在沙箱里跑一条命令、输出进/不进上下文」的
  路径；后台命令可以复用同一套执行与显示，只是把「等它结束」换成「结束再报」。ADR 0033 决定 6 说
  「同一时间只允许一条；运行中 Esc 取消」——后台命令会打破「只允许一条」，需要在本 ADR 里重新定。
- **取消（Esc/Ctrl-C）**：现在的取消是一个 `Effect::Cancel` → `SessionCommand::Cancel`，取消的是
  **turn**（在飞的模型调用/工具）。后台命令不属于任何 turn，需要一个「按 job id 取消」的动作，
  以及「会话退出时取消还在跑的 job」的路径（omp 的 `dispose`、dsh 的「owner/harness 关闭即结束」）。
- **journal 审计**：后台命令的启动与结束各一条 durable 记录（照 ADR 0002 的 marker 规则），恢复时
  「启动过但没结束」的后台命令如何处理是恢复语义的一部分（孤儿 job：重启后是继续等、报一次
  「unknown after crash」，还是丢弃——ADR 0002 对工具调用已有 `outcome_unknown_after_crash` 的先例）。

## 4. 未核实与待决定

**未核实**：

1. Claude Code 会话结束时如何清理还在跑的后台进程（类型定义只有 `backgroundEndsWithFinalResponse` 与
   `worker_restart` 两个边界）。
2. omp job 子进程的进程组/setsid 细节（`bash.ts` 的 `setsid` 一类没有逐行读）。
3. opencode 会话结束时后台进程的清理，以及 `persistBackground` 在重启后如何与模型对上。
4. dsh 的 `jobs-local` 到底怎么把 job 绑到进程（同进程内），以及并发/输出上限的默认值。
5. pi 是否有**扩展**真的实现了后台命令（随包只看到 plan-mode 的注释提到 background task
   notifications）。

**待 决定**（ADR 0037 里逐条给推荐）：见 `docs/adr/0037-background-commands.md`。
