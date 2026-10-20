# orca v0 设计稿＼�2026-10-20）

状态：**定为 v0，作为 idea 记录，不开工。**替代 herdr；不是 daemon。将来要 desktop / iOS / web app（见末节）。

## 角色

- **Claude（lead）**：分析需求、制定计划、拆成 item，交给 orca；看 gate 判定和 diff，决定返工或交 owner 合并。
- **orca（项目经理）**：backlog 目录的**唯一写者**。接收 item、按依赖分发、收结果、起 gate、记录机器与 session id。确定性程序，不是模型。
- **worker**：一次性 `radian exec -p`，一个任务一个进程、一个 session、一个 worktree；跑完退出；只读，没有输入口（headless 无 prompter，`ask_user` 不挂载，ADR 0043 §4）。
- **gate**：可配置的阶段序列（commands / mutation / review / ci / human），每个阶段一次性运行、只读，写判定。机械部分归它，判断归 Claude，merge 归 owner。
- **人类**：`radian attach <session-id> [--follow]` 只读观察（ADR 0043）；直接读 backlog 目录。

## backlog：目录，单写者

```
<backlog>/
  lock                      # orca 的独占锁（flock）；拿不到锁就拒绝启动
  todo/<id>.md              # brief：目标、读哪些文件、步骤、限制、验收命令、after: [...]、host 偏好
  doing/<id>.md             # 同一文件被 orca mv 过来，附 run 元数据
  review/<id>.md            # worker 退出且 Result 为 success 后 mv；等 gate
  done/<id>.md | failed/<id>.md
  runs/<id>/
    <n>.result.jsonl        # worker stdout 的最后 Result 记录（ADR 0044），n 为第几轮（返工递增）
    <n>.stderr.log
    meta.toml               # session_id、host、worktree、branch、commit、started/finished
    verdict.<n>.md          # gate 的判定
```

- 状态 = 文件所在目录；每次状态变化是一次 `rename`，原子。
- **只有 orca 写这个目录**。worker 和 gate 的结果走 stdout 回到 orca，由 orca 落盘。Claude 通过 orca 的工具写，不直接碰文件。
- 不依赖 git 分支：backlog 可以不在 git 里；要历史就让 orca 把目录当一个单分支仓库，只有它 commit。代码分支数量与 backlog 稳定性无关。

## orca 的两个入口

1. **MCP stdio server**（Claude Code 的 `.mcp.json` / radian 的 `agent-extensions` 都能接）：
   `add(item)`、`list(state?)`、`show(id)`、`rework(id, brief)`、`cancel(id)`。
   `add` 校验 brief 有验收命令和 `after` 引用存在，否则拒绝。
2. **`orca run`**：取锁；循环：挑 `todo` 里依赖已 `done`、host 可用的 item → mv 到 `doing` → 派发 → 收 Result → mv 到 `review` → 起 gate → 收 verdict → `done`/`failed`。队列排空就退出。重启只看目录，不丢状态。

## 派发

**不变量＼�：一个 work item 一个 session。** session id 就是 item id；首轮新建，返工 `--resume` 同一个；一个 item 永远不会跑在别的 session 里，一个 session 也永远只属于一个 item。推和拉都不改这条：会循环的只有脚本和定时器，没有任何 radian 会话跨 item 存活。

v0 推，v1 拉；`radian work` 两种都接，它只收一份 brief 加 item id，来源是 stdin 还是 HTTP 对后面没有区别。

### v0：推

```
ssh <host> 'radian work --stdin' < brief            # 首轮
ssh <host> 'radian work --stdin --resume <id>' < rework-brief   # 返工，必须回同一台
```

`radian work`（先是脚本）：建 `.radian-worktrees/<id>` 与分支 `work/<id>` 自 `origin/main`（返工时复用）；
`radian exec -p - --session-id <id> --workspace <worktree> --yes --output jsonl`；
把最后一行 Result 加上 commit id 原样打到 stdout；退出码透传（ADR 0044 §3）。
worker 只 commit 不 push（AGENTS.md 规则不变）；orca 在收到 success 后 `git push origin work/<id>`，gate 才能在别的机器取到。

### v1：拉（需要 `orca serve`）

每台机器一个定时器（launchd/systemd timer，不是 agent）跑 `radian work --pull <orca-url>`：向 orca 要**一个** item；orca 作为唯一写者决定给不给（mv 到 `doing`，记 host），给了就返回 brief 和 item id；脚本为它起一个新 session，跑完把 Result 回传给 orca，退出。没活就立刻退出。本机容量由脚本自己判断（例如正在跑的 `radian exec` 进程数 < N 才去领）。返工也是拉：orca 把 rework 只发给原 host 的拉请求。

运行控制（抄 Symphony SPEC 的语义）：
- **并发上限**：全局一个数，可按 host 再设上限；没有空位的 item 留在 `todo`。
- **stall 超时按无输出计**，不按总时长：orca 读 worker 的 `--output jsonl` 流，超过 `stall_timeout` 没有新行就 SIGTERM（radian 退出码 143，Result 的 outcome 为 cancelled，ADR 0044 §3），记一次失败。
- **失败退避**：worker 非零退出或 stall → 指数退避（10s 起，上限 5min）后同一 item 重试，次数上限后进 `failed`；每次尝试一个新的 `runs/<id>/<n>.*`。重试用 `--resume`，让模型看到上一轮。
- **重启**：orca 不留内存状态。启动先扫 `doing/`：meta.toml 里的进程还活着就接管等待，死了就按失败处理；然后照常循环。

## gate：可配置的阶段序列＼�

- 触发：item 进 `review`。gate 不是一个 agent，是一串阶段；每个阶段一次性运行，产出 `verdict.<n>.<stage>.md`，全部通过才 `done`。
- 配置两级：orca 的缺省序列，brief 里的 `gates:` 可覆盖或追加（只能加严，不能删掉缺省里标 `required` 的阶段）。
- 阶段种类：

| 种类 | 做什么 | 跑在哪 | 输入 | 输出 |
|---|---|---|---|---|
| `commands` | 跑验收命令（Verify 七条，`--no-fail-fast`，易抖的重复 N 次） | 独立 worktree，按 commit 检出 | brief 的验收命令 | 每条命令的退出码与尾部输出 |
| `mutation` | 按 brief 声明的行为逐个改坏、跑指定测试二进制、恢复、比对 | 同上，但与 `commands` 不同树、不同时 | brief 的 `mutations:`（行为、改坏方式、应红的测试） | 每个变异被哪个测试抓住；没被抓住的列为 hole |
| `review` | 一个只读 radian 会话读 diff、brief、worker 自报的不确定项；核对每条"保证"类注释；不改代码 | 任意机器，只读 | diff、brief、Result | 结构化意见：阻塞 / 建议 / 已核对 |
| `ci` | orca 推分支、开 PR（或直接等远端 checks），等 checks 结束 | GitHub | 分支 | checks 结论与链接 |
| `human` | 停在这里等人：owner 看 attach 或 PR 后点 pass/fail | 无 | 前面所有 verdict | 人工结论 |

- 缺省序列（radian 仓库）：`commands(required)` → `mutation(required)` → `review` → `human`。`ci` 对 radian 可选，因为 Verify 已经等价于 CI；别的仓库可能只有 `ci` + `human`。
- 失败处理按阶段配置 `on_fail`：`rework`（orca 用 verdict 自动生成返工 brief，`--resume` 派回原机器）或 `hold`（停在 `review`，等 Claude 或人决定）。缺省：`commands`/`mutation` 失败 → `rework`；`review` 阻塞 → `hold`。返工次数上限后 → `failed`。
- 独占：`commands` 与 `mutation` 跑时该 host 不派发别的任务；两者绝不同树（memory：变异与 Verify 不能同树、同机并行）。
- 判断仍归 Claude：`review` 阶段的意见是输入，不是裁决；merge 归 owner。

## 不做

- 常驻进程：orca 排空即退，worker/gate 一次性。
- worker 中途插话：纠正走 `rework`（`--resume`）。
- worker 写 backlog 或读 backlog 路径：brief 永远是 stdin 的一份拷贝。
- 进 `radian` 二进制：先做仓库外原型（同 acp backlog 的先例）；坐实后新 ADR、改 ADR 0032/0043 再进。

## 已验证的事实（2026-10-20，本机）

- `radian exec`：`-p -`、`--session-id`、`--resume`、`--workspace`、`--yes`、`--output jsonl`（Result 含 outcome/turns/cost）。
- `radian attach`：只读、不开 socket、`--follow`。
- Claude Code 2.1.304 的 `-p --input-format/--output-format stream-json` 存在，但本设计不用它当 worker。

## 未验证 / 待定

- `--session-dir` 放同步目录能否跨机器 resume（未验证；设计先绑机器）。
- 变异脚本的格式：brief 里怎么声明"行为 → 改坏方式 → 应红的测试"。
- orca 原型语言（shell + 少量 Python，或直接 Rust 小 crate 在仓库外）。
- 若将来要一个会判断的"项目经理"模型：就是一个挂了 orca MCP 工具的 radian 会话，工具面不变。

## 用户视角：跑起来的步骤

### 一次性准备（每台机器）

1. 每台 worker 机器装好 `radian`，`radian login <provider>` 存好 key（`~/.radian/credentials/auth.json`，ADR 0026）；clone 仓库到固定路径。
2. Mac 到每台机器的 ssh 免密（LXC、Hai-PC 已有）。
3. 装 `orca`（原型期是一个目录里的脚本；进二进制后是 `radian orca`）。写一份 `orca.toml`：
   ```toml
   backlog = "~/.radian/orca/radian"        # 单写者目录
   repo    = "git@github.com:hisea/radian.git"
   [hosts.mac]   ssh = "localhost"            path = "~/Code/radian"        max = 1
   [hosts.lxc]   ssh = "<user>@<linux-host>"    path = "~/radian"             max = 2
   [hosts.pc]    ssh = "<user>@<windows-host> -p 2222" path = "~/radian"         max = 1
   stall_timeout = "10m"
   gates = ["commands:required", "mutation:required", "review", "human"]
   ```
4. 把 orca 的 MCP server 加进 Claude Code（`claude mcp add orca -- orca mcp --config orca.toml`），这样 Claude 多出 `add/list/show/rework/cancel` 五个工具。

### 每个需求的循环

1. **你**对 Claude 说需求。Claude 分析、读 ADR、给你一份计划：几个 item、依赖顺序、每个的验收命令和变异声明。你看完点头（或改）。
2. **Claude** 调 `add` 把 item 写进 backlog。你用 `orca list` 能看到 `todo/` 里的文件。
3. **你**起 `orca run`（或者 launchd 定时器每 5 分钟起一次）。它取锁，按依赖派发，排空就退。终端里每行一个事件：派到哪台、session id、退出码、进了哪个 gate。
4. **想看 worker 在干什么**：`ssh <host> radian attach <id> --follow`，只读。或者直接 `cat ~/.radian/orca/radian/runs/<id>/1.stderr.log`。
5. **gate 跑到 `human`** 时 orca 停在那里并打印：item、commit、分支、前面各阶段 verdict 的路径。你看 diff（`git fetch && git diff origin/main...work/<id>`）或 attach 看过程。
6. **你**决定：`orca pass <id>` 或 `orca fail <id> "原因"`。fail 会让 Claude（或你自己写）一条 `rework`，orca 下次 run 时 `--resume` 派回原机器。
7. **合并**还是你今天的流程：Claude 给你分支名，你在 detached worktree 里 `pull --ff-only` 再合到 main 推 origin。orca 看到 item 的分支已进 main 就把它 mv 到 `done/`，删 worktree。

### 日常只需要记的四条命令

```
orca run                 # 推进队列直到排空
orca list                # 看每个 item 在哪个目录
orca pass <id> / fail    # human gate 的答复
radian attach <id>       # 看一个 worker（在它跑的机器上）
```

### 没写进设计、但用户会碰到的

- `orca run` 在 Mac 上跑时，Mac 自己也是 worker 机器，Verify 和变异不能和你手里的工作同机并行（memory 的教训）：缺省 `hosts.mac.max = 1`，并且 gate 阶段优先派到 LXC。
- worker 失败三次进 `failed/` 后不会自动重试，要人或 Claude 重新 `add`。
- 第一次用某台机器前，手动 `ssh <host> radian exec -p "hi"` 确认 key 和模型可用；orca 不做这个检查。

## v1 方向：desktop / iOS / web app＼�

v0 是终端加 Claude。将来要一个 app 看 backlog、看 worker、答 human gate。对手里 Orca（onorca.dev）、OrchestratorInc、Nimbalyst 都已带 iOS 伴侣 app，这是桌面管理器那一类的标配。v0 现在就要留三条缝，否则 v1 要重写：

1. **数据格式就是 API。** app 渲染的是 backlog 目录里的东西：item 的 frontmatter、`meta.toml`、`<n>.result.jsonl`、`verdict.<n>.<stage>.md`。v0 把这些字段定下来并写 schema（和仓库里 `docs/schema/` 同一做法），之后只加不改。
2. **orca 要有 `serve` 模式。** 远程 UI 必须有东西可连，而 v0 的 `orca run` 排空即退、锁也随之释放。v1 加 `orca serve`：持有锁、暴露一个本地 HTTP/WebSocket（只在 loopback 或 ssh 转发上，ADR 0013 的不出网缺省不变），同一套 add/list/show/rework/cancel/pass/fail，再加订阅事件流。`run` 和 `serve` 共用同一个状态机，`run` 等价于"serve 到排空就退"。这一条和研究报告"不做 client/server"的结论不冲突：那条说的是单会话 radian 本体，这里是 orca。
3. **观察流要可转发。** app 里"看 worker"就是 `radian attach --follow` 的输出，v1 由 `orca serve` 代理到各 host 的 attach，所以 v0 的 attach 输出最好已经是 jsonl 而不是渲染后的文本（待核实 attach 今天输出什么）。

v1 不定的事：桌面用 Tauri 还是 web 套壳；iOS 是原生还是 PWA；远程访问走 Tailscale/ssh 转发还是自建中继（后者碰 ADR 0013）。这些等 v0 跑满一个真实需求再说。
