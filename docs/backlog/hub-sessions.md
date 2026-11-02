# hub 会话面：手机看、开、接管 radian 会话

- 状态：decided（radian 侧三件事与 iOS 侧一件事；三个小项待决见末尾）
- 涉及 ADR：0043（控制口动词、来源规则、「控制口只做本机 IPC」、`attach` 看不到流式输出）、0011（L2 来源）、0012（权限来源）、0023（U7 队列、折叠代码的归属）、0024（第二个前端答问题）、0005（消息 `origin` 元数据）、0004 与 0026（模型热切换、`/model` 的列表）、0010（committed 与 live 事件）、0048（§16 hub）、0020（`run/`、`hub/` 的沙箱可读性）、0013（不新增出站连接）、0034（通知内容的隐私口径）、0049（不动：ACP 不参与）
- 来源：`arc-ios.md` 待决第二条（第二版是否加实时会话）；hub 落地后的讨论

## 背景

hub 今天只管 arc：loopback、bearer token、`/v1/repos`、SSE 与 owner 动作。手机要的是 radian 会话本身，分三档：

- **看**：列出某仓库的会话、读 transcript、跟着正在跑的会话刷新。
- **开**：从手机新开会话、发消息、看回答、答审批。
- **接管**：往 Mac 上 TUI 正跑的那个会话里发消息、答它的审批。两个前端，同一个会话。

三档都要。会话已有三个接缝：journal 与只读的 `radian attach`；每个会话进程的控制口 `~/.radian/run/<pid>.sock`（`followup`、`steer`、`inject`、`cancel`、`list_sessions`、`subscribe`，有界扇出）；`radian acp`。接管只能走控制口，所以三档统一走控制口，hub 是手机唯一的门，ACP 不参与。控制口离接管只差两件事：以 `User` 身份发消息，和答审批。审批队列在 host 层（ADR 0023 U7），`answer(request_id, resp)` 先答者赢、晚到者什么都不改，`radian acp` 已经在 TUI 之外用它答审批，所以「手机答、Mac 的弹窗消失」在机制上是现成的。

## 决定（已定）

### 1. 身份：hub 的 token 就是 owner

- 控制口的请求可带一个可选字段 `token`。值等于 `~/.radian/hub/token` 的内容时，这个请求的输入来源记为 `User`，并开放三个新动词 `pending`、`answer`、`config` 与一个 `models`。没有 `token` 或 `token` 不对的请求和今天完全一样：来源缺省 `Scheduler`，新动词被拒，什么都不改。
- ADR 0043「来源由端口定，不由客户端自称」不破：端口凭 token 定。token 在 arc 里已经代表 owner（能 merge），同一凭据授权「替 owner 说话」是一致的。
- 安全口径：token 文件沙箱不可读（ADR 0020）；`run/` 与 socket 保持沙箱可读（它只是进程坐标）。沙箱里的 shell 能连上 socket，但没有 token 就拿不到任何新权力。`full` 档没有沙箱，模型的 shell 本来就能自己跑 `radian`，这里不增加新权力（ADR 0043 对 `full` 档的前提）。
- ADR 0043「控制口与 herdr 报告是本机 IPC，不做远程协议」改为：控制口是本机 IPC，hub 是它唯一的远程入口；远程面仍是 hub 已有的 loopback 加 token，radian 不新增任何监听口与出站连接（ADR 0013）。

### 2. 控制口的扩展（`radian-host`）

- `followup`、`steer`、`inject` 带 `token` 时来源为 `User`；消息的 `origin` 元数据记 `{"via": "hub"}`（ADR 0005 G1），TUI 在 transcript 里把这样的消息标成来自手机。排队语义不新设计：回合中 `steer`，回合外 `followup`，与键盘输入相同。
- 输入可带附件：`images`（base64 加 MIME）与 `files`（文件名加文本）。它们走 TUI 拖入图片和 `@` 文件的同一条路（`mention::user_message`）变成消息的 ContentPart，图片上限与 TUI 的 composer 相同，文件压缩时的折叠规则不变（`[file: P, N lines]`）。一个请求是一条 JSON 行，行上限写进手册。
- `pending`：返回队列头与队列长度。审批带工具名、解析后的资源、原始参数；结构化提问带 ADR 0024 的完整 `QuestionRequest`；都带 `request_id`。
- `answer { request_id, response }`：审批答 `allow_once`、`allow_always`（本进程内的 `AllowForSession`，与 ACP 面相同）或 `reject_once` 加原因；提问答完整的 `QuestionAnswer`，自由文本、多选、`Other` 都能答（比 ACP 面强）。晚到（`Answered::Late`）与未知 id 返回错误行，什么都不改。
- `config`：一个 JSON 对象送进 ADR 0004 的 `ConfigQueue`，在回合边界应用并记 `ConfigChanged`，与 `/model` 同一条路。v1 只接受 `model`（`{ "provider", "model" }`）与 `thinking`。
- `models`：返回这个会话 `/model` 会列出的同一份列表（已登录 provider 的模型，按 provider 分组，当前路由带标记；ADR 0026 D5）。
- `subscribe` 在今天的 committed `AgentEvent` 与 `RuntimeEvent` 之外，多推一种 `pending` 事件：队列的版本号变了就推当前的头（或空）。仍然只推 committed 事件，不推逐 token 的增量（ADR 0043：`attach` 看不到流式输出）。
- `list_sessions` 的那一行加上 `workspace`、当前模型路由、回合状态（`idle`、`working`、`blocked`，按 ADR 0034 H2 的分类）与 `pending` 数。

### 3. hub 的会话面（`radian-arc` 的 hub）

- **发现**：hub 扫 `~/.radian/run/*.sock`，对每个 socket 调 `list_sessions`，按 `workspace` 归到登记的仓库；没登记的 workspace 不出现。已结束的会话用 `radian exec --list` 的同一套查找（按 workspace）。hub 对每个活着的会话长期保持一个 `subscribe`，断了就重连；这个订阅同时喂 SSE 与第 5 条的钩子。
- **路由**（`api_version` 升到 2；1 的路由与行为不变，只读客户端照旧能用）：

| 路由 | 方法 | 内容 |
|---|---|---|
| `/v1/repos/<name>/sessions` | `GET` | 该仓库的会话：id、创建时间、最近活动时间、标题（第一条用户消息）、`live`（`tui`、`hub` 或 `none`）、回合状态、`pending` 数、当前模型路由。 |
| `/v1/repos/<name>/sessions/<id>/transcript` | `GET` | hub 侧折好的 transcript entry 列表。 |
| `/v1/repos/<name>/sessions/<id>/events` | `GET` | SSE：entry 的新增与更新、`pending`、`config`、回合状态。与 `/v1/events` 同一条规则：不重放，断线重连后先重读 transcript。 |
| `/v1/repos/<name>/sessions/<id>/models` | `GET` | 控制口 `models` 的结果。 |
| `/v1/repos/<name>/sessions` | `POST` | 新开会话：`text`，可选 `model`、`images`、`files`。hub 起一个进程（第 4 条），返回会话 id。 |
| `/v1/repos/<name>/sessions/<id>/prompt` | `POST` | `text`、可选 `steer`（回合中插入）、`images`、`files`。会话活着就经它的 socket 发；已结束的会话由 hub 以 `--resume` 起进程后再发。 |
| `/v1/repos/<name>/sessions/<id>/cancel` | `POST` | 控制口 `cancel`。 |
| `/v1/repos/<name>/sessions/<id>/answer` | `POST` | 控制口 `answer`；晚到答 `409`，什么都不改。 |
| `/v1/repos/<name>/sessions/<id>/config` | `POST` | 控制口 `config`。 |
| `/v1/repos/<name>/items/<task>/conversation` | `GET` | arc worker 的对话，同一种 entry 格式；补上 hub 第一版不看实时会话的缺口。 |

- **折叠在 hub 侧做**，手机只认一种 entry 格式。三种来源折成同一种：活着的会话的 committed 事件、journal、arc attempt 的 `result.jsonl`（ACP）。`attach` 的折叠与 `AcpFold` 从 `radian-tui` 挪到非 TUI 的 crate，TUI 与 hub 共用；这是 ADR 0023 的边界改动。
- **一个 journal 同时只有一个进程**。hub 起进程前先扫 socket；TUI `--resume` 一个 hub 进程持有的会话被拒并指出是 hub 在持有，与「另一个 workspace 的会话」被拒是同一类消息。

### 4. hub 持有的会话

- hub 起的是不带 `-p` 的 `radian exec`：进程打开会话（新建或 `--resume`）后不跑回合，等自己控制口的输入。它是普通的 general 会话（ADR 0043），workspace 是仓库路径，配置来自这台机器，模型来自请求的 `model`，没给就按配置。
- 手机离线后进程继续活着。没有回合在跑、没有待答、没有后台 job 的时间超过 `hub.session_idle_secs`（缺省 3600，`60` 到 `86400`）就退出；journal 留着，下一条消息以 `--resume` 再起。
- 回收只对 hub 自己起的进程。TUI 持有的会话由 TUI 的进程决定生死，hub 只经它的 socket 读和发。

### 5. 通知

- hub 加 `[hub.hooks] after_pending`：某个活着的会话的队列出现新的头时跑一次，环境变量给仓库名、会话 id、种类（`approval` 或 `question`）和工具名；不给命令文本、路径与问题正文（ADR 0034 的口径）。手机点开后经 hub 取正文。
- 它跑在 hub 进程里，依赖第 3 条的长期订阅；hub 不在时没有通知。会话钩子的 `PermissionRequest` 时间点（`hooks-permission-request.md`）是另一件事，不互相依赖。

### 6. 手机端（`radian-ios`，v1 之后的第二个 plan）

- 每个仓库一个会话列表；transcript 页；composer：文本、`+` 选照片或文件、模型选择（来自 `models`）、发送；审批与提问的 sheet（提问含自由文本与多选）；待答角标；「已在 Mac 上答复」状态；通知深链到会话页。
- 交接不需要动作：一个会话就是一个 journal。TUI 持有时，手机和 TUI 是同一个进程的两个前端；TUI 退出后由 hub 持有；TUI `--resume` 回来时 hub 的进程必须已经退出（第 3 条末尾）。

### 要改的 ADR

0043（§1 控制口那条与「本机 IPC」那句）、0012 与 0011（带 token 的端口输入是 `User`）、0023 U7 与 0024（第二个前端答队列）、0005（`via` 元数据）、0048 §16（hub 的会话面；建议把 hub 独立成一个 ADR）、0020（`run/` 保持可读的理由、`hub/` 新钩子）。0049 不动。

## 待决问题

- `subscribe` 要不要一个 `live: true` 可选项推逐 token 的增量：用过 committed 粒度再说。
- 要不要 `after_idle`（回合结束且无待答）钩子。
- `models` 的列表是否也在 hub 自己算（它读得到同一份配置与凭据），省掉一次 socket 往返：等实现时看。

## 验收

- 控制口：无 `token` 的 `answer`、`pending`、`config` 被拒且什么都不改；带 `token` 的 `followup` 在 journal 里的来源是 `User`、`origin.via` 是 `hub`；带错误 `token` 的来源仍是 `Scheduler`。
- 审批：手机经 hub 答队列头后，TUI 的弹窗消失、工具按答复执行（pty 测试）；TUI 先答，hub 的 `answer` 返回 `409` 且不改结果；自由文本提问从 hub 答到，tool 结果与键盘答的一致。
- 沙箱：`workspace` 档里模型的 shell 读不到 `hub/token`（已有的不可读测试覆盖）；它连上 socket 后的无 token `answer` 被拒。
- 附件：带图片与文件的 `followup` 进 journal 的消息与 TUI 拖入同样内容的消息逐 part 相同。
- 模型：`config` 切换后下一回合用新路由，journal 有 `ConfigChanged`；回合中切换不影响在飞的请求（ADR 0004 D4）。
- hub：两个登记仓库各一个活着的 TUI 会话，`sessions` 列表的 `live` 与状态正确；SSE 在一条消息落盘后到达一个 entry；断线期间的消息重连后重读 transcript 能看到；对已结束会话的 `prompt` 起一个 hub 进程并 `--resume` 同一个 journal；空闲超过 `hub.session_idle_secs` 后进程退出且 journal 完整；TUI `--resume` 一个 hub 持有的会话被拒并指出持有者。
- 折叠：同一 journal 经 `radian attach` 与经 hub 的 `transcript` 得到相同的 entry 序列；arc attempt 的 `result.jsonl` 经 `conversation` 的 entry 与 arc TUI 的对话页相同。
- 钩子：`after_pending` 在一个新的队列头出现后跑一次，环境变量只含仓库名、会话 id、种类与工具名。
- 手册：每个新动词、字段、路由、配置键与钩子时间点都写出名字、类型、缺省与取值集；ADR 按上面的清单改成新的快照。
- 手测（写进 `crates/radian-tui/README.md` 与 `crates/radian-arc` 的清单）：Mac 上开一个 TUI 会话，手机在 tailnet 内发一条消息、答一次审批、换一次模型、发一张照片，TUI 同步可见；手机新开一个会话、关 app 一小时后再打开，会话仍在。
