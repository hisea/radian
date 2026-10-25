# arc：hub 接口与通知钩子（远程客户端的服务端）

- 状态：decided（派发前要定的小事见末尾；通知钩子排在 `arc-landing.md` 落地之后，复用它的 `[hooks]`）
- 涉及 ADR：0048（§11 owner 动作、§13 调度循环与 `runner.lock`、§14 v0 排除项、§15 配置与目录）、0013（新的监听口）、0020（`~/.radian/` 下的新文件）、0008（新配置键）、0034（通知内容的隐私口径）
- 来源：`docs/backlog/arc.md` 里「v1 要 desktop / iOS / web app」一条；`research/2026-10-22-arc-app-product-design.md`；客户端本身见 `arc-ios.md`

## 背景

arc 的全部状态是 `<repo>/.radian/arc/` 下的文件，单写者。调度循环是每个仓库一个的 `radian arc serve`，持有该仓库的 `runner.lock`；TUI 与 MCP 服务器不持有 `runner.lock`，只拿 `state.lock`，TUI 的 owner 动作直接调 CLI 的库函数（ADR 0048 §11）。今天没有监听口，手机想看 plan 与 task、做 owner 动作、锁屏收通知，只能 ssh 进来跑 CLI。

## 决定（已定）

### 1. 一个 hub 管所有仓库

新增一个进程，管本机登记过的所有仓库（不是每个仓库各开一个口）。它和 TUI、MCP 服务器是同一类：

- 读：直接读各仓库的 arc 文件，返回与 `list --json` / `show --json` 相同的结构。
- 写：owner 动作调 CLI 的同一组库函数，只拿该仓库的 `state.lock`；拿不到锁时返回 CLI 的那句消息，什么都不改。
- 它不持有任何 `runner.lock`，不跑调度循环，不启动循环。某仓库没有循环在跑时，接口如实报告（读 `runner.lock` 是否被持有），客户端显示出来。
- 仓库登记是显式列表，不自动发现。

hub 只是又一个读者和 owner 动作的入口，不是新的调度者，所以 ADR 0048「单写者、单循环」不变。

### 2. 只听 loopback，token 必须

- 默认只绑 `127.0.0.1`。暴露到 tailnet 由 `tailscale serve` 或 ssh 端口转发完成，radian 不管。
- 每个请求都要 bearer token，读写都要；没有关闭 token 的选项。Tailscale 的身份头不作为凭据。
- token 第一次启动时生成，存在 `~/.radian/` 下 sandbox 不可读的位置（ADR 0020）。有一条命令打印连接地址加 token（含二维码），给客户端配对用；还有一条命令换新 token，旧 token 立即失效。
- 按 ADR 0013，这是一个新的网络面：只在用户启动 hub 时存在，radian 不主动发起任何出站连接。

### 3. 接口（v1）

路由带 `/v1/` 前缀，另有一个整数 `api_version`；`list` / `show` 的结构仍由 `schema_version` 管（ADR 0048 §5）。

读：

- 仓库列表：每个仓库的名字、路径、循环是否在跑。
- 某仓库的 plan 与 task：`list --json` 的结构。
- 某个 plan 或 task：`show --json` 的结构。
- task 的最终报告：最新 attempt 的 `result.jsonl` 最后一条 assistant 消息（与 ADR 0048 §7 交接段读的是同一处）。
- task 的 gate 结论：各 stage 的 verdict。
- task 的 diff：先返回文件列表与每个文件的增删行数，再按文件取内容；单个文件太大时截断并注明。

写（owner 动作，与 TUI 同一组库函数，同样的校验与报错）：

- `confirm`、`merge`、`reject`（必须带非空 note）、`cancel`、`auto`、`pause`、`reverify`、`priority`。
- `start`、`reset-budget` 不在 v1。
- merge、reject、把 auto 调到 `merge`、cancel 的二次确认由客户端做，接口不做。

事件：

- 一条 SSE 流，覆盖所有登记的仓库，一个事件一条状态迁移，带仓库名、task 或 plan id、旧值与新值；plan 的迁移与 MCP channel 推送用同一套字段。
- 事件来源复用 `radian arc mcp --claude-experimental` 的轮询与差分代码，扩展成多仓库，不另写一套。
- 事件只是提示，不是 durable 的：没有事件 id 重放，不处理 `Last-Event-ID`。客户端断线重连后先重新读一遍列表，再接着收事件。

v1 不提供会话的实时查看：不投影 journal，不发 block，不需要把 TUI 的投影代码挪出 radian-tui。

### 4. 通知走钩子，radian 不直接发推送

- 锁屏通知由钩子命令发：用户在命令里自己调 Bark、ntfy 或别的推送服务，radian 不内置任何推送服务，也不持有 APNs 密钥。
- 钩子是 `arc-landing.md` 的 `[hooks]`（同一组环境变量，同样的超时、输出与"失败只警告"规则），在调度循环里跑，所以 hub 不在时通知照样发。这里加两个时间点：`after_awaiting_human`（进入 `review.awaiting_human` 之后）、`after_proposed`（新 task 进入 `todo.proposed` 之后）。`after_fail`、`after_rework`、`after_merge` 已在 `arc-landing.md`。
- 隐私口径（ADR 0034）：手册的示例钩子只发仓库名、task id 与 phase，正文由客户端打开后连 hub 取。示例里通知点开后跳到客户端的 task 页（客户端的 URL scheme 见 `arc-ios.md`）。

## 派发前要定的小事

- 子命令名（建议 `radian arc hub`：`radian arc serve` 已是调度循环），配对与换 token 的子命令名，默认端口。
- 仓库登记放在哪：`~/.radian/config/config.toml` 的一节，还是 `~/.radian/` 下单独一个文件；token 文件的路径。两者都要改 ADR 0020 的目录表。
- 用哪个 HTTP 服务端 crate（工作区今天没有）。
- `api_version` 与 `schema_version` 之间的关系写在哪一页手册。

## 验收

- 集成测试起一个 hub，登记两个临时仓库：读接口的结构与 `list --json` / `show --json` 逐字段相同；diff 的文件列表与按文件读取、大文件截断；最终报告取的是最新 attempt 的最后一条 assistant 消息。
- 每个 owner 动作经接口调用后，仓库文件的变化与 CLI 做同一个动作完全相同；reject 空 note 被拒；拿不到 `state.lock` 时返回 CLI 的消息、文件不变。
- 没有 token、token 错误、换 token 后的旧 token：都返回 401，什么都不改。默认只绑 loopback。
- SSE：两个仓库各做一次迁移，两条事件都到达且字段正确；断线期间的迁移，重连后读列表能看到。
- 钩子：`after_awaiting_human`、`after_proposed` 在对应迁移后各跑一次，环境变量都到达；hub 不在时钩子照常运行。
- 手机在 tailnet 内不装 ssh 客户端，能完成浏览、owner 动作与锁屏通知（手测，写进 `crates/radian-arc` 的手测清单）。
- ADR 0048 新增 hub 一节并在 §14 写明 hub 不是调度者；ADR 0013 写明新监听口；ADR 0020 目录表加新文件；手册写出每个子命令、参数、配置键、钩子时间点与接口版本。
