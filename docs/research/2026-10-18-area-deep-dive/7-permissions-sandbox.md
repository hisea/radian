# 深挖 7：权限与沙箱的交互摩擦——Radian 与五个对手

2026-10-18 · Radian `hisea/radian@356df1e` · 对手源码：codex `8c07694`、opencode `63c1a27`、pi-mono `482c138`、omp 18.12.1 npm 包；Claude Code 只用官方文档和 Anthropic 工程博客（每条标"文档"）。`4-architecture.md` 已经写过的三点不再展开：`workspace` 和 `full` 两档的 shell 都是 `ask`；会话授权按完整命令串精确匹配；非 auto 时 `general` subagent 的 ask 一律被拒。本文往下挖它们的成因、对手的具体机制，以及每条改法的安全论证。

## 1. 一句话结论

Radian 的权限层在**安全语义**上是五家里最严的之一：deny 优先，非人来源只降不升，探测失败就拒绝，绝不降级。它的问题是**没有任何一条路径能在不降低安全的前提下少问一次**。沙箱不能免掉审批；复合命令一律不算简单命令；授权只能精确到整条命令串，而且活不过这个会话；后台通知或 herdr 发起的回合碰到 ask 直接拒绝；subagent 既拿不到父会话的授权，也没有审批人。dogfood 里 53% 的工具调用是 shell（253/479，`docs/research/2026-10-15-tool-concurrency-measurement.md` 合计表），所以缺省的 `full + on-ask` 下，模型每两次工具调用就有一次要停下来等人，最后大家只好用 `--yes`。`--yes` 等于什么都不问，比"在沙箱里不问"更不安全。

对手的共同做法是**把"问不问"交给一个更硬的边界去决定**。Codex 和 Claude Code 让沙箱代替人：命令在沙箱里跑就不问，越界时才问（Codex `core/src/exec_policy.rs:828-842`；Claude Code 文档 sandboxing#auto-allow-mode）。opencode 和 Claude Code 按子命令解析，按前缀加项目持久化来记住授权。Claude Code 的 auto 模式另用一个分类器模型替人判断。

Radian 最该做的五件事都不削弱现有的不变量：
1. 在 `workspace` 档，命令在沙箱里执行就自动放行（Linux 要先补 Unix socket 的洞）。
2. 沙箱拒绝的命令要分类，模型显式请求越界时再问人。
3. 复合命令按段判断 allow。
4. 授权改成"前缀 + 禁止前缀清单"，并在宿主侧按项目持久化。
5. subagent 继承父会话的 allow 规则和授权，ask 经控制 socket 转回父会话去问。

## 2. 对比表

| 维度 | Radian | pi | Codex CLI | Claude Code（文档） | omp | opencode |
|---|---|---|---|---|---|---|
| 档位 / 缺省 | `safe`/`workspace`/`full`，缺省 `full`（`agent-cli/src/profile.rs:79-81`）；审批缺省 `on-ask`（`radian-permissions/src/profile.rs:249`） | 不分档，不问（`coding-agent/docs/security.md:3`） | 沙箱缺省 `read-only`（`protocol/src/config_types.rs:104-107`），审批缺省 `on-request`（`protocol/src/protocol.rs:1104-1107`） | 交互会话缺省 `auto`（v2.1.283 起），另有 `default`/`acceptEdits`/`plan`/`dontAsk`/`bypassPermissions`（permission-modes#available-modes） | `tools.approvalMode` 缺省 `yolo`（`src/tools/settings.ts:311-315`） | 缺省规则 `* allow`，只有读 `.env` 要问（`packages/schema/src/agent.ts:46-51`） |
| OS 沙箱后端 | macOS seatbelt；Linux bwrap，没有 seccomp（`radian-sandbox/src/bwrap.rs:121-131`）；探测失败就拒绝 | 无；示例扩展用 `@anthropic-ai/sandbox-runtime`（`examples/extensions/sandbox/index.ts:1-30`） | seatbelt；Linux 是 bwrap + seccomp（`linux-sandbox/README.md:10-14`），seccomp 禁 ptrace、io_uring，并按模式限制 socket 族（`linux-sandbox/src/seccomp.rs:193-279`） | seatbelt；bwrap + socat，可选的 seccomp 用来挡 Unix socket（sandboxing#set-up-linux-and-wsl2）；沙箱缺省关 | 无（`src/` 里没有找到 seatbelt 或 bwrap 后端） | 无（`core/src` 里没有找到 OS 沙箱后端） |
| 沙箱能否免审批 | **不能**，档内 shell 固定是 `ask`（`profile.rs:144-151`） | — | 能：`on-request` + 受限沙箱 + 不危险 → `Allow`（`exec_policy.rs:828-842`） | 能：`autoAllowBashIfSandboxed` 缺省 true（sandboxing#auto-allow-mode） | — | — |
| 越界怎么升级 | 没有机制，也不区分是沙箱拒绝还是普通失败（`radian-coding` 里找不到 denial 分类） | — | 模型带上 `sandbox_permissions: require_escalated` + `justification` 重发，然后问人（`core/src/tools/handlers/shell_spec.rs:225-259`）；`on-request` 下不自动重试（`core/src/tools/orchestrator.rs:439-460`） | 模型带 `dangerouslyDisableSandbox` 重试，按当前模式决定问人还是交给分类器；可以用 `allowUnsandboxedCommands:false` 关掉（sandboxing#the-unsandboxed-retry-escape-hatch） | — | — |
| 允许的粒度 | 规则：`CommandPrefix` 逐 token 匹配（`shell.rs:44-50`）；会话授权：完整命令串（`hooks.rs:238`、`408-413`） | — | execpolicy 前缀规则；模型可以提议 `prefix_rule`，有禁止清单（`exec_policy.rs:57-…`、`977-996`） | `Bash(npm test *)`；提示框给出 "don't ask again for: npm test *"（permissions#permission-system） | 每个工具 allow/prompt/deny，加 bash 模式规则（`src/tools/bash.ts:504-560`） | 按 ARITY 表推导前缀，例如 `cargo add *`、`git status *`（`core/src/shell/parse.ts:23-…`、`375-381`） |
| 持久化 | 只有 `--allow-shell-command` 命令行参数，不进配置；授权不跨 resume（`radian-permissions/README.md`） | — | 追加到 `~/.codex/rules/default.rules`（`exec_policy.rs:54-56,875`） | 写进仓库根目录的 `.claude/settings.local.json`，按仓库生效（permissions#permission-system） | 写进设置文件（未细核） | 存进 SQLite，按 `projectID` 区分（`core/src/permission.ts:147-155,298-305`） |
| 复合命令 | 出现任何元字符就不算简单命令 → ask（`shell.rs:22-35`）；deny 会拆开逐段查（`shell.rs:52-94`） | — | tree-sitter 解析，只接受由 `&& \|\| ; \|` 连起来的纯单词命令，逐段判断（`shell-command/src/bash.rs:22-60`） | 识别 `&& \|\| ; \| \|& &` 和换行，每段都要命中 allow；剥掉 `timeout`/`nice` 等包装（permissions#compound-commands） | 只接受 `&&` 连接的纯 argv，可选开启、缺省关（`src/exec/settings.ts:94-97`、`shell-tokenize.ts:233-243`） | tree-sitter 逐个 `command` 节点生成资源，每段各自评估（`parse.ts:174-200`） |
| 网络 | `safe` 关；`workspace` 全开，只能全开或全关（ADR 0016；`docs/sandbox.md` 未验证一节） | — | `workspace-write` 缺省关网（`protocol.rs:1208-1219`）；有 HTTP/SOCKS 代理做域名策略（`network-proxy/README.md`） | 全部走本机代理，allowlist 起始为空，未知域名按模式决定是问人还是拒绝（sandboxing#hosts-outside-your-allowed-domains） | 不限 | 不限 |
| 一次打断的选项 | `[y]` 一次、`[a]` 本会话、`[n]` 拒绝、`[!]` 本会话转 auto（`radian-tui/src/render/approval.rs:13-14`） | — | 批准、批准并写入前缀规则 | 一次 / 前缀永久 / 转 auto / 拒绝；拒绝时可以附一句话（permissions#add-a-comment…） | 未细核 | 一次 / always（按项目）/ 拒绝；拒绝可附反馈，会**整批**拒掉同会话的其余请求（`permission.ts:276-296`），always 会顺手放行其他待决请求（`308-321`） |
| 无头缺省 | `exec -p` 无终端时 ask → 拒绝，并提示加 `--yes`（`agent-cli/src/approval.rs:35-40`）；config 里的 `auto` 在 `full` 档被忽略（`radian-host/src/approval.rs:59-68`） | 不问 | `exec` 固定 `Never`：命令在沙箱里跑，失败原样还给模型（`exec/src/lib.rs:588-591`、`exec_policy.rs:816-820`） | `dontAsk` 拒绝所有需要提示的调用；`--allowedTools` 做精确白名单（permission-modes#common-setups） | yolo | `run` 不带 auto 时拒绝，并给模型反馈"继续，不做这一步"（`cli/src/run/noninteractive.ts:73-74,158-185`） |
| 子 agent 继承 | 继承档位；只有父会话是 auto 时才传 `--yes`；不传 allow 规则，也不传授权（`radian-host/src/subagent.rs:390-405`） | 不内建 | 继承父会话的 `approval_policy` 和 permission profile（`core/src/agent/child_config.rs:178-201`） | auto 模式下分类器在三个时点检查子 agent（permission-modes#how-auto-mode-handles-subagents）；后台子 agent 的提示转回主会话（sub-agents） | 进程内 | 子会话的请求沿 `parentID` 归给根会话，由根会话应答（`noninteractive.ts:144-156`） |

## 3. 每家的关键做法

### Codex CLI——沙箱就是缺省的审批人
- **判断顺序**：先用 tree-sitter 把命令拆成纯单词段，不能证明是纯单词的命令整体当作一段（`shell-command/src/bash.rs:22-60`）。然后 execpolicy 逐段匹配（`exec_policy.rs:376-380`）。没有规则命中的段交给 `render_decision_for_unmatched_command`：如果是危险命令（`rm -f`、`sudo` 等，`command_safety/is_dangerous_command.rs:142-147`），就**一定问人**，在 `Never` 下直接禁止；否则在 `on-request` + 受限沙箱下**直接放行**，在不受限的环境里也放行（`exec_policy.rs:802-842`）。
- **越界走显式升级**：shell 工具多了三个参数：`sandbox_permissions`（`use_default` / `with_additional_permissions` / `require_escalated`）、`justification`（直接作为问人的文案），以及 `prefix_rule`（`shell_spec.rs:225-259`）。沙箱拒绝的判断是启发式的：先看退出码，再看 stderr/stdout 里有没有 `operation not permitted`、`read-only file system`、`sandbox` 等 7 个关键词，SIGSYS 对应 seccomp（`sandboxing/src/denial.rs:13-71`）。在 `on-request` 下判定为沙箱拒绝后**不会自动去沙箱重跑**，只把精简后的拒绝信息交还给模型，由模型决定要不要带 `require_escalated` 重发（`orchestrator.rs:439-460`，`sandboxing.rs:345-352`）。
- **可复用授权**：模型可以提议一个前缀规则，人批准后追加到 `default.rules`。有一份**禁止前缀清单**，包括 `bash -c`、`sh`、`node -e`、`python`、`env`、裸 `git` 等，这些前缀即使提议了也不保存（`exec_policy.rs:57-…`、`989-996`）。只有当**每一段**都被显式规则允许时，命令才能绕过沙箱（`exec_policy.rs:446-460`）。
- **无头**：`exec` 固定 `Never`，命令在沙箱里跑，失败原样交给模型，从不升级（`exec/src/lib.rs:588-591`）。
- **网络和 Linux 强度**：`workspace-write` 缺省不开网（`protocol.rs:1218`），要开网就经过代理做域名策略（`network-proxy/README.md`）。seccomp 在代理模式下只放行 IP socket 去连本地桥，独立的 AF_UNIX 要显式授权；所有模式都禁 `io_uring`，防止绕过 socket 族限制（`seccomp.rs:197-270`）。
- **自动审查**：`approvals_reviewer = auto_review` 把越界请求交给一个子 agent 做风险判断（`protocol/src/config_types.rs:183-200`）。

### Claude Code（文档）——沙箱、规则、分类器三层
- **auto-allow**："Claude Code approves a command automatically, with no prompt, when the command runs inside the sandbox"。即便如此，下面这些仍然生效：deny 规则；按内容写的 ask 规则（例如 `Bash(git push *)`）；针对关键路径的 `rm`；`FOO=bar cmd`、`make CC=…`、`eval` 这类会改变实际执行程序的形式仍要过常规审批（sandboxing#auto-allow-mode）。
- **网络**：沙箱命令只能连本机代理，allowlist 起始为空；未知主机在 Manual 模式下问人，在 auto 模式下拒绝，除非命令自己列出要连的主机并经分类器批准；同一份列表只对这一条命令有效（sandboxing#per-command-allowed-domains-in-auto-mode）。代理还拒绝解析到本地地址的主机名，例如 `169.254.169.254`。文档明确承认存在域名前置（domain fronting）风险（sandboxing#security-limitations）。
- **子命令粒度**：每段都要命中 allow，任何一段命中 deny 或 ask 就生效（包括子 shell 和 `$(…)` 里的命令）。"don't ask again" 对复合命令按**段**保存，最多 5 条；先剥掉 `timeout`/`time`/`nice`/`nohup`/`stdbuf`/`command`/`builtin` 再匹配；`direnv exec`、`npx`、`docker exec` 这类不剥（permissions#compound-commands、#process-wrappers）。有一组内建的只读命令在所有模式下都不问：`ls cat grep find wc diff stat`、只读的 git 等；对 glob、`ps e`、`cd`+`git` 这类组合另有附加条件（permissions#read-only-commands）。
- **项目规则要过信任门**：项目 `.claude/settings.json` 里的 `allow` 只有在用户接受信任对话框之后才生效；`deny` 和 `ask` 不受影响，因为它们只收紧（permissions#project-allow-rules-and-workspace-trust）。
- **auto 模式**（分类器）：用户批准了 93% 的提示。分类器分两级：第一级在真实流量上的 FPR 是 8.5%；完整流水线的 FPR 降到 0.4%，但对真实"过度积极"动作的 FNR 是 17%。连续 3 次或累计 20 次被拦就退回问人。分类器看不到工具结果，以防注入（[anthropic.com/engineering/claude-code-auto-mode](https://www.anthropic.com/engineering/claude-code-auto-mode)）。进入 auto 模式时会丢掉 `Bash(*)`、`Bash(python*)`、包管理器的 run 这类等于任意代码执行的宽规则（permission-modes#how-the-classifier-evaluates-actions）。
- **CI**：`claude -p … --permission-mode dontAsk --allowedTools "Bash(npm test)"`。完全无人值守用 `--dangerously-skip-permissions`，而且要求外层有容器或 VM（permission-modes#common-setups）。

### opencode——不设沙箱，靠解析和持久化减少打断
- 规则按"最后一条命中者生效"（`permission.ts:88-97`）。缺省规则是 `* allow`（`schema/src/agent.ts:46-51`），所以实际上几乎不问。
- shell 命令用 tree-sitter 拆成每个 `command` 节点，每个节点生成一个 `resource`（原文）和一个 `save`（按 ARITY 表推导的前缀加 ` *`，例如 `cargo` 取 2 个词，`cargo add` 取 3 个），见 `parse.ts:174-200,375-381`。选 always 时把 `save` 存进 SQLite，按项目区分（`permission.ts:298-305`），并且**立刻**放行同一时刻其他满足条件的待决请求（`308-321`）。
- 拒绝时附一句话会变成 `CorrectedError` 交给模型，模型可以接着改方案；同会话里并行的其他请求一并拒掉，"parallel asks don't end the step"（`permission.ts:276-296`）。
- 无头 `run`：不带 `--auto` 时一律拒绝，并给模型反馈 "This non-interactive run cannot ask… Continue without this action"，回合不结束（`noninteractive.ts:73-74,163-182`）。子会话的请求沿 `parentID` 递归找到根会话，由根会话应答（`144-156`）。

### omp 与 pi
- omp 把工具分成 read/write/exec 三级，按 `always-ask`/`write`/`yolo` 三种模式决定问不问，缺省是 yolo（`settings.ts:307-320`）。有一组"关键模式"（例如 `curl … | sh`、`mkfs`、`dd of=/dev/`）在非 yolo 模式下强制问；**yolo 下不生效**，因为 yolo 分支在检查 override 之前就返回了（`approval.ts:261-279`）。复合命令按段审批，但这个功能缺省关闭，而且只认 `&&`（`exec/settings.ts:94-97`）。
- pi 什么都不问，安全交给容器或专用 OS 用户（`docs/security.md:3-19`）。权限和沙箱都是可选扩展：`permission-gate.ts` 在没有 UI 时直接拦截危险命令；`sandbox/` 扩展用的是 Claude Code 开源的 `sandbox-runtime`。

## 4. Radian 的优势（不能为了快丢掉的）

1. **执行边界不能被审批扩大，探测失败绝不降级**（ADR 0012 P1、决定 2；`agent-cli/src/profile.rs:12-14`）。Claude Code 在沙箱起不来时缺省会照常运行（要加 `failIfUnavailable` 才拒绝，sandboxing 文档）；Radian 缺省就拒绝。
2. **非人来源只降不升**（ADR 0012 决定 5；`hooks.rs:236-264`）。后台通知（`InputSource::System`，`radian-host/src/job_notify.rs:149`）和控制口消息（缺省 `Scheduler`，`control.rs:715`）拿不到人的授权。opencode 和 omp 都没有这一层。
3. **deny 匹配很保守**：拆开分隔符、引号和子 shell 逐段查，`/bin/rm` 也算 `rm`（`shell.rs:52-103`）。README 把 shell matcher 看不到的东西写得很清楚，例如 `git -c core.fsmonitor=…`、`cargo build` 会执行 build.rs（`radian-permissions/README.md`）。
4. **`.git` 执行入口只读**（ADR 0038 决定 4），`deny_read` 覆盖会话、记忆、凭据和信任库（`chat.rs:39-60`）。Claude Code 把同类内容叫作 protected paths，但那是权限层的检查；Radian 是 OS 层强制。
5. **按项目记住的档位存在宿主侧，并且沙箱禁读**（`~/.radian/access/access.json`，`docs/sandbox.md` 运行中切换一节）。仓库里的文件只能收窄（ADR 0043 决定 2.3）。Claude Code 把永久授权写进仓库目录下的 `.claude/settings.local.json`，再靠 protected-path 检查挡住模型改它。Radian 的做法更硬。
6. **沙箱开销小**：每条命令约 5.9 ms（macOS）和 8.0 ms（Linux）（`docs/sandbox.md` 启动开销一节）。拿它来代替一次人工审批（秒到分钟级）几乎不要钱。

## 5. 劣势和缺口（4-architecture 之外的部分）

1. **沙箱只算成本、不算收益，这是写死在档位里的**：`workspace_write()` 和 `full_access()` 里的 shell 规则都是 `Action::Ask`（`profile.rs:144-151,162-168`）。而且 `--allow-shell-command` 的实现方式是**删掉**这条整体 ask 规则，再加 allow（`agent-cli/src/profile.rs:102-129`），说明这套规则模型本身就没有"沙箱内放行"的位置。
2. **default `full` 下没有任何免问路径**。Codex 在不受限环境 + `on-request` 下，不危险的命令直接放行（`exec_policy.rs:826-831`）；Claude Code 有一组内建只读命令。Radian 在 `full` 下连 `git status` 都要问，除非用户每次都手写 `--allow-shell-command`。配置文件里没有对应的 allow 列表（`docs/schema/config/` 31 个 fixture 都没有），config 里的 `approval = "auto"` 在 `full` 档还会被忽略（`radian-host/src/approval.rs:59-68`）。这两点加在一起，**持久的"少问"只剩 `--yes` 或会话内 `/auto on` 两条路**，两条都是全有或全无。
3. **复合命令永远不能被 allow**：`is_simple` 要求整串都在安全字符集里（`shell.rs:22-35`），所以 `cargo fmt && cargo test`、`cd crates/x && cargo test`、`rg foo | head` 永远要问。模型自然写出的命令里这类很常见（未量化；建议用 journal 里的 `PermissionResolved` 加命令文本统计一次）。
4. **授权活不过会话，而且是精确字符串**：key 是 `(tool, 完整命令串)`（`hooks.rs:238`、`408-413`），提示框里的 `[a]` 只对一模一样的命令有效。opencode 和 Claude Code 给的是前缀，而且按项目持久化。
5. **沙箱拒绝和普通失败分不开**：`radian-coding` 没有对 EPERM、只读文件系统这类失败做分类。模型看到的只是一个失败，可能反复重试同一条命令，也没有一个"请求越界"的正规通道。
6. **非人来源的回合碰到 ask 只能拒绝**：后台任务完成通知（ADR 0037）和 herdr 控制口驱动的回合，只要不在 auto 下就不能跑任何 shell（`hooks.rs:250-264`）。人就在 TUI 前面，也没有机会回答。
7. **subagent 拿到的权限比父会话少**：子进程只拿到 `--profile` 和（父会话为 auto 时的）`--yes`。父会话的 `--allow-shell-command`、会话授权和档位都**不传**（`subagent.rs:390-405`，文件里 `allow_shell` 出现 0 次）。所以父会话里已经放行的 `cargo test`，在子会话里也会被拒。
8. **Linux `workspace` 档有一个和"免审批"直接冲突的洞**：bwrap 共享主机网络命名空间，路径 Unix socket 经只读根挂载可达，抽象 socket 也可达（`docs/sandbox.md` 未验证一节的表），`[sandbox] allow_unix_sockets` 在 Linux 上不生效。也就是说，沙箱里的命令能连 `/var/run/docker.sock`，这等于主机 root。今天这个洞被"每条 shell 都问人"挡着；一旦做沙箱内免审批，它就成了真正的逃逸路径。macOS 那边 `mach-lookup` 全开，是 ADR 0012 决定 4 已经接受的风险，但接受的时候前提是还有人工审批这一层。
9. **网络只能全开或全关**：`workspace` 开网（ADR 0016），所以沙箱挡不住"读工作区里的 `.env` 再 curl 出去"。Codex 缺省关网，Claude Code 走 allowlist 代理。

## 6. 改进方案（按收益/成本排序）

### ① `workspace` 档：沙箱内的 shell 自动放行（收益最高；成本中；**带 Linux 前置条件**）
- **做什么**：在 `workspace_write()` 里把 shell 的 `Ask` 换成一个新动作 `AllowIfSandboxed`。只有同时满足两条时，它才在 `decide` 里变成 allow：(a) 本次调用实际拿到的 `SandboxSlot` 是强制后端，不是 `inherited`，也不是 `full` 的透明后端；(b) 调用没有带越界请求（见 ②）。其他情况一律退回 `ask`。`deny` 照旧最先生效。`ApprovalPolicy::Always` 照旧可以把它改回 ask，给想逐条审的人用。
- **安全论证**：
  - ADR 0012 P1 说"approval 不能扩大沙箱"。这条改动只是在沙箱**之内**不问，执行边界不变，所以不违反 P1。
  - 相比今天实际在用的 `full + --yes`，这是严格变安全：一样不问，但多了一层 OS 边界。
  - 相比 `workspace + 逐条问`，丢掉的只是人对"沙箱内能做的事"的复核。这些事是：写工作区、读非禁读区域、联网。其中**联网外泄工作区秘密**是真实的损失（缺口 9），文案必须照实说，ADR 0012 决定 4 的表述约束不变。
  - **Linux 前置条件**：缺口 8 必须先补，否则 auto-allow 在 Linux 上等于把 docker.sock 交给模型。补法任选其一：参考 Codex `seccomp.rs:197-270` 加一个 seccomp 过滤，禁止 AF_UNIX `connect`（白名单路径除外）并禁 `io_uring`；或者在补上之前，**Linux 上 auto-allow 不启用**，启动行写明原因。
  - `inherited` 后端（ADR 0016）看不到外层边界，不放行。
- **为什么更快**：53% 的工具调用是 shell。做完之后 `workspace` 档的提示次数会降到"越界请求 + MCP"，`workspace` 档也才第一次比 `full + --yes` 更好用。
- **ADR**：修订 0012 的档位规则（P1 原文写的是"仍显示审计与确认"）；涉及 0016（Linux socket）、0043 决定 2（缺省档位不变，但要重新评估，见 ⑧）。
- **验证**：在 `workspace` 下跑 `cargo test`，journal 里的 `PermissionResolved.approver = policy`，并且不出提示。变异测试：探针失败或用 `inherited` 时仍然放行，测试应失败；Linux 上没有 seccomp 时放行，测试应失败。A/B：同一组任务统计提示次数和墙钟时间。

### ② 沙箱拒绝分类 + 显式越界请求（收益高；成本低到中；依赖 ①）
- **做什么**：shell 工具对强制档位里失败的命令做分类：后端已知（seatbelt 或 bwrap），退出码非 0，输出命中 EPERM、`Read-only file system`、`Operation not permitted`（参考 Codex `denial.rs` 的 7 个关键词），就在结果里加一行 `sandbox: likely denied (<原因>)`，并说明可以带 `escape_sandbox: true, justification: "…"` 重发。带这个参数的调用一律走 `ask`，提示框直接显示 justification。`never` 和无头模式下就是 deny，原因交还给模型（与 Codex 的 `exec` 一致）。**绝不自动去沙箱重跑**（Codex `on-request` 也不这么做，`orchestrator.rs:439-460`）。
- **安全论证**：越界始终要人说 yes。参数本身不授予任何东西：越界请求在 `decide` 里最多得到 `ask`，`deny` 规则照旧生效，非人来源照旧拒绝。启发式误判只影响文案提示，不影响放行决策。可以加一个用户级开关 `[sandbox] allow_escape = false`，相当于 Claude Code 的 strict 模式。
- **为什么更快**：模型不再反复重试一条注定会被沙箱拒绝的命令，而是一次说清楚为什么要越界。人只在真正需要的地方被打断。
- **ADR**：新 ADR，或者并入 ① 的那份修订；工具 schema 变了，涉及 0036 修订 3（工具表每个会话只算一次）。**验证**：写 `~/x` 时结果带标记；带参数重发出现 ask；`--approval never` 下被拒；变异测试：去掉 ask 改成直接执行，测试应失败。

### ③ 复合命令按段判断 allow（收益中高；成本低）
- **做什么**：新增 `shell::segments(command) -> Option<Vec<String>>`。只在**未加引号**的 `&&`、`||`、`;`、`|` 处切分；每一段仍必须满足今天的 `is_simple`，任何其他元字符都让整串返回 `None`，也就是退回今天的行为。复合命令只有在**每一段**都命中 allow 时才 allow；deny 的逻辑完全不动。再加一张"执行其参数"的程序表，例如 `xargs`、`env`、`sudo`、`nohup`、`time`、`timeout`、`watch`、`sh`/`bash`。表里的程序出现在段首时，这一段不能靠前缀规则 allow（参照 Claude Code 剥包装的清单、Codex 的 `BANNED_PREFIX_SUGGESTIONS`）。`cd <路径>` 只有在目标解析后仍在工作区内时才算可放行的段（Claude Code 对 `cd`+`git` 另有限制，因为 git 会跑目标目录的 hooks；Radian 在 `workspace` 档里 `.git/hooks` 只读，但 `full` 档里没有这层保护，所以 `full` 下 `cd` 段一律不放行）。
- **安全论证**：每一段满足的条件和今天单条命令一样严，管道传的只是数据。剩下的风险，即 `a | xargs rm` 这种段尾执行，由"执行其参数"的程序表挡住。这是一个纯 Rust 的小函数，不需要 tree-sitter 依赖。它只可能让 allow 命中得更少（多了一个"每段都要命中"的条件），不会让 deny 漏判。
- **ADR**：`radian-permissions/README.md` 的 shell 一节要改，不推翻任何 ADR 的决定（ADR 0012 只规定了 allow/ask/deny 的语义）。**验证**：沿用 `shell_metacharacters_are_never_allowed`；新增 `a && b` 里 b 没有规则时 ask、`ls | xargs rm` 时 ask；变异测试：改成"任一段命中就 allow"，测试应失败。

### ④ 授权：前缀加禁止清单，按项目持久化在宿主侧（收益中高；成本中）
- **做什么**：在提示框新增一个选项 `[p] always "<前缀> *" in this project`。前缀用一张类似 opencode ARITY 的小表推导（`cargo test`、`git status`、`npm run build` 这种程序加子命令），而且**必须显示在提示框里**（与 Claude Code 一样："只提供能完整展示的选项"）。禁止清单直接复用 Codex 的思路：裸解释器、`sh -c`、`bash`、`env`、`xargs`、裸 `git`、裸 `cargo`、裸 `npm`、`python`、`node -e` 都不能作为前缀，碰到就只提供"一次"。这些规则写进 `~/.radian/access/` 下按项目的文件（与 access.json 同类，沙箱禁读，仓库里的任何文件都不能写它），启动时作为 `allow` 规则加载；用 `/permissions` 列出和删除。现有的 `[a] 本会话` 也从精确串改成同一个前缀。
- **安全论证**：
  - 持久化的是**人亲手批准的、显示出来的前缀规则**，和 `--allow-shell-command` 同一类东西，不是 ADR 0012 P2 禁止持久化的"会话授权"。
  - 规则存在宿主侧，并且沙箱禁读，模型和仓库都改不了。
  - 前缀按 token 匹配，README 已经论证过：`allow git status` 不会匹配 `git -c … status`。
  - 宽前缀等于任意代码执行，这个风险由禁止清单挡住。
  - 非人来源的回合照样不能借用会话授权（ADR 0012 决定 5）。但持久规则属于"宿主的 policy"，按 README 的说法对所有来源都生效，这一点要在新 ADR 里写明并由 拍板。保守的做法是：项目持久规则也只对人发起的回合生效。
- **为什么更快**：一个项目里重复出现的命令（`cargo test -p x`、`cargo clippy …`）只问一次。opencode 和 Claude Code 都这么做。
- **ADR**：新 ADR，涉及 0012 P2、0020（`~/.radian/` 布局）、0004（实时状态）。**验证**：resume 之后 `cargo test -p b` 不再问；`bash -c …` 只提供"一次"；变异测试：禁止清单失效时测试应失败；从仓库内写规则文件被沙箱拒绝。

### ⑤ subagent 继承父会话的放行能力，ask 转回父会话（收益中高；成本：第一步低，第二步中）
- **第一步（低成本）**：子进程的 argv 加上父会话当时生效的全部 shell allow 规则，包括 `--allow-shell-command`、④ 的项目规则，以及父会话里人给的前缀授权（作为 allow 规则传下去）。档位照旧取父会话当时的档位。① 做完之后，`workspace` 档的子会话自动获得沙箱内放行。
  - **安全论证**：子会话能做的事是父会话**不问人**就能做的事的子集。任务文本是父会话模型写的，同一个模型在父会话里本来就能直接用这些授权，所以权力没有增加。ADR 0043 1a 的"不宽于父"仍然成立。传下去的只有 allow 规则，不传 `--yes`。
- **第二步（中成本）**：子会话里仍然需要问的调用经控制 socket 转给父会话的 approver，提示框标注来源 `subagent <id>`，这正是 opencode（`noninteractive.ts:144-156`）和 Claude Code 后台子 agent 的做法。父会话本身无头的时候，照旧拒绝。
  - **安全论证**：回答的仍是同一个人，提示框写明了来源，等于一次新的、知情的人工决定，不是借权。控制口今天"拒绝 user 来源与审批动词"（ADR 0043 冻结 4），所以要新开一条**由父进程发起**的反向通道，而不是让外部客户端获得审批动词。
- **ADR**：修订 0043 1a.7。**验证**：父会话 `--allow-shell-command "cargo check"` 时，`general` 子会话里的 `cargo check` 成功；子会话里的 `cargo publish` 被拒（第一步）或出现在父会话的提示框里（第二步）。变异测试：子会话拿到 `--yes`，测试应失败。

### ⑥ `full` 档：一组内建的只读命令免问（收益中；成本低；**缺省档位直接受益**）
- **做什么**：内建一张小的只读表，`git status|diff|log|show|branch --list`、`ls`、`pwd`、`wc`、`rg`/`grep` 这类，要求所有路径参数解析后都在工作区内、不带 `..` 越界、不带可执行或可写的选项（例如 `find -exec`/`-delete`、`git -c`、`--output`）。在 `full` 和 `workspace` 两档都是 allow。
- **安全论证**：这些命令不写入，只读工作区，而内建的 `read`/`grep` 工具本来就能读到同样的东西（ADR 0039 的边界），所以模型能看到的内容没有变宽。路径限制保证 `cat ~/.ssh/id_rsa` 仍然要问。可执行选项按"出现即不算只读"处理，失败方式是多问一次，和 README 里"over-asking is the intended failure mode"一致。Claude Code 在所有模式下都这么做（permissions#read-only-commands）。
- **为什么更快**：`full` 是缺省档。做了 ① 也帮不到 `full` 用户，只有这一条能直接减少缺省档的打断。**未量化**：只读命令在 253 次 shell 里占多少，需要从 journal 统计。
- **ADR**：修订 0012 的档位规则（可以和 ① 合成一份修订）。**验证**：每类可执行选项一条拒绝用例；路径越界时 ask。

### ⑦ 非人来源回合的 ask：人在场就问，不在场就拒（收益中；成本低）
- **做什么**：今天 `!human && !task_approver` 时直接 deny（`hooks.rs:250-264`）。改成：如果 host 有交互 approver（TUI），就照常弹框，但提示框写明 `this turn was started by: background job notice / herdr`，并且**只提供 `[y] once` 和 `[n]`**，不提供会话授权或 auto。无头时照旧 deny。
- **安全论证**：ADR 0012 决定 5 防的是**借用**人的授权，也就是在没有人参与的情况下得到 allow。这里每次都由人当场确认，并且人知道来源，不存在借权。不提供 `[a]` 和 `[!]`，是为了让非人来源不能把一次批准放大。还要防止刷屏：同一个非人回合里第二次被拒之后，后面的 ask 自动 deny。
- **ADR**：修订 0012 决定 5（只改"ask 怎么处理"，不改"只降不升"）。**验证**：后台通知触发的回合里 shell 出现提示，提示框里没有 `[a]`；无头时拒绝。

### ⑧ 中期：网络 allowlist 代理，并重新评估缺省档位（收益中；成本高）
- 让 `workspace` 档的网络走本机代理，按域名放行，起始只放行常用的包仓库。参照 Claude Code 的代理（起始为空，拒绝解析到本地地址的主机名，例如 169.254.169.254）和 Codex 的 `network-proxy`。这能补上 ① 留下的外泄损失（缺口 9），也让"无人值守用 `workspace`"真正站得住。成本大：要新写一个代理组件、处理 `socat` 一类转发，还要面对 ADR 0043 决定 2.2 的"不再为沙箱加能力"，必须由 owner 重新决定。
- ①②③⑥ 落地、0043 背景里那四条接缝修完之后，用 A/B 数据（提示次数、墙钟时间、失败率）重新评估缺省档位是不是应该回到 `workspace`。这会推翻 0043 决定 2.1，需要新 ADR。

### ⑨ 不建议做的
- **分类器 auto 模式（Claude Code）**：每条 shell 多一次模型往返，费用计入用量；完整流水线对真实过度积极动作的 FNR 是 17%（工程博客）。对一个不绑定 provider、追求低延迟的单二进制来说，先用沙箱代替人更便宜，也更确定。等 ①–④ 做完再看剩下的提示量值不值得用分类器。
- **让模型自己提议并持久化前缀规则（Codex `prefix_rule`）**：④ 由宿主推导前缀、人确认就够了，没有必要再把一个可写面交给模型。
- **沙箱拒绝后自动去沙箱重跑**：Codex `on-request` 都不这么做，Claude Code 也要走审批或分类器。
- **opencode、omp 那样缺省全放行**：ADR 0043 已经选了 `full + on-ask`。在 `full` 档减少打断应该靠 ⑥ 这种可证明的子集，而不是放宽缺省。
- **用 Landlock 代替 bwrap**：ADR 0012 决定 2 已经论证过 Landlock 表达不了"可写工作区里的 `.git` 只读"。Landlock 只作补充加固。真正缺的是 seccomp 层的 socket 限制（①）。

## 7. 验证总表（每条落地后都要跑 AGENTS.md 的 Verify）

| 方案 | 关键断言 | 必做的变异测试 |
|---|---|---|
| ① | `workspace` 下的沙箱 shell 记 `approver=policy`；`inherited` 或探针失败时 ask；Linux 没有 socket 防护时不放行 | 去掉"实际在沙箱内"的判断 |
| ② | 沙箱拒绝带标记；越界参数 → ask；`never` → deny | 越界直接执行 |
| ③ | `a && b` 在 b 无规则时 ask；`… \| xargs rm` 时 ask；引号内的 `&&` 不切分 | 任一段命中就 allow |
| ④ | resume 后前缀仍然生效；禁止前缀只提供"一次"；规则文件沙箱禁读 | 禁止清单失效 |
| ⑤ | 子会话继承 allow；子会话不继承 `--yes` | 子会话拿到 `--yes` |
| ⑥ | `git -c`、`find -exec`、越界路径都要 ask | 去掉选项检查 |
| ⑦ | 非人来源回合的提示框里没有 `[a]`、`[!]`；无头时拒绝 | 非人来源回合可以选 `[a]` |

量化建议：在 ①③④⑥ 做之前，先写一个小脚本扫 `~/.radian/sessions/` 的 journal，统计 `PermissionResolved` 里 `approver=user` 的命令有多少是复合命令、有多少是只读命令、有多少能被前缀覆盖。用这个数据决定 ③④⑥ 的先后顺序（今天只知道 shell 占工具调用的 53%）。
